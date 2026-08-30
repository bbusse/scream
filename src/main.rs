// scream - Screen Stream
//
// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2026 Björn Busse

use clap::Parser;
use gstreamer::{self as gst, prelude::*};

use scream::dlna;
use scream::http::Request;
use scream::layout::Grid;
use scream::metrics::{
    self, ClientGuard, CLIENTS_MJPEG, CLIENTS_MKV, CLIENTS_RTSP, CLIENTS_SNAPSHOT,
    CLIENTS_WEBM, SNAPSHOTS_TOTAL,
};
use gstreamer_app as gst_app;
use gstreamer_rtsp_server::prelude::*;
use gstreamer_rtsp_server::{RTSPMediaFactory, RTSPServer};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::os::unix::io::AsFd;
use std::io::Write as _;
use std::collections::VecDeque;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_output::{self, WlOutput},
    wl_registry::{self, WlRegistry},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

// CLI

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
enum Mode {
    /// Capture a whole compositor output (monitor)
    Output,
    /// Capture each window separately and composite them
    Window,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Capture mode
    #[arg(long, value_enum, default_value = "output")]
    mode: Mode,
    /// Address to bind the RTSP server to
    #[arg(long, default_value = "0.0.0.0")]
    bind_address: String,
    /// Port to bind the RTSP server to
    #[arg(long, default_value = "7001")]
    bind_port: String,
    /// Composite canvas width; defaults to wl_output width (toplevel mode only)
    #[arg(long)]
    width: Option<u32>,
    /// Composite canvas height; defaults to wl_output height (toplevel mode only)
    #[arg(long)]
    height: Option<u32>,
    /// Frames per second to capture and encode
    #[arg(long, default_value = "30")]
    framerate: u32,
    /// Port for the http mjpeg stream a browser can display, 0 disables it
    #[arg(long, default_value = "7002")]
    http_port: u16,
    /// Port the media player relays stream audio to, opus over rtp; 0 disables
    #[arg(long, env = "STREAM_AUDIO_PORT", default_value = "7005")]
    audio_port: u16,
    /// Do not announce the stream over ssdp for dlna players
    #[arg(long)]
    no_dlna: bool,
    /// The name dlna players list the stream under
    #[arg(long, default_value = "ISS Display")]
    dlna_name: String,
}

static DLNA_NAME: OnceLock<String> = OnceLock::new();

fn dlna_name() -> &'static str {
    DLNA_NAME.get().map(|s| s.as_str()).unwrap_or("ISS Display")
}

static FRAMERATE: AtomicU32 = AtomicU32::new(30);

fn framerate() -> u32 {
    FRAMERATE.load(Ordering::Relaxed)
}

fn frame_interval() -> Duration {
    Duration::from_millis(1000 / framerate().max(1) as u64)
}

// A short queue that drops the oldest frame rather than block: the live
// sources and the compositor must never wait on each other, and 200 ms of
// slack is enough for the encoder and the rtsp appsink to meet their deadline
fn make_queue() -> gst::Element {
    gst::ElementFactory::make("queue")
        .property("max-size-time", 200_000_000u64)
        .property("max-size-bytes", 0u32)
        .property("max-size-buffers", 0u32)
        .property_from_str("leaky", "downstream")
        .build()
        .expect("queue")
}

// Shared types

#[derive(Clone)]
struct LatestFrame {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    gst_format: &'static str,
}

type FrameSlot = Arc<Mutex<Option<LatestFrame>>>;

// Capturing an output costs a few percent of a core; encoding it is what costs
// real time. So one capture feeds every consumer and the encoders start only
// when something asks for them: h264 when an rtsp client connects, jpeg while
// an http client is reading
static FRAMES: OnceLock<FrameSlot> = OnceLock::new();
static CAPTURE_RUNNING: AtomicBool = AtomicBool::new(false);
static CAPTURE_SHUTDOWN: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn frames() -> &'static FrameSlot {
    FRAMES.get_or_init(|| Arc::new(Mutex::new(None)))
}

fn capture_shutdown() -> &'static Arc<AtomicBool> {
    CAPTURE_SHUTDOWN.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

fn ensure_capture() {
    if CAPTURE_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let slot = frames().clone();
    let shutdown = capture_shutdown().clone();
    std::thread::spawn(move || wayland_capture_loop_output(slot, shutdown));
}

// The media player relays the current stream's audio here, opus over rtp. One
// capture thread decodes it to raw pcm; the rtsp pay1 and every webm client
// read chunks from the queue, padding silence when nothing is playing
type PcmQueue = Arc<Mutex<VecDeque<Vec<u8>>>>;
static AUDIO_PCM: OnceLock<PcmQueue> = OnceLock::new();
static AUDIO_CAPTURE: AtomicBool = AtomicBool::new(false);
// Set once from Args so the http handlers can reach it; 0 disables audio
static AUDIO_PORT: AtomicU16 = AtomicU16::new(0);

fn audio_port() -> u16 {
    AUDIO_PORT.load(Ordering::Relaxed)
}

// 20 ms of 48 kHz interleaved stereo s16, the opusdec output chunk
const AUDIO_CHUNK_BYTES: usize = 960 * 2 * 2;

fn audio_pcm() -> &'static PcmQueue {
    AUDIO_PCM.get_or_init(|| Arc::new(Mutex::new(VecDeque::new())))
}

fn audio_caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", 48_000i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build()
}

// One buffered chunk, or silence when the queue is empty
fn next_audio_chunk() -> Vec<u8> {
    audio_pcm()
        .lock()
        .ok()
        .and_then(|mut q| q.pop_front())
        .unwrap_or_else(|| vec![0u8; AUDIO_CHUNK_BYTES])
}

fn ensure_audio_capture(port: u16) {
    if port == 0 || AUDIO_CAPTURE.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let desc = format!(
            "udpsrc port={port} \
             caps=application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload=97 \
             ! rtpjitterbuffer latency=200 do-lost=true \
             ! rtpopusdepay ! opusdec plc=true \
             ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! appsink name=asink sync=false max-buffers=64 drop=true"
        );
        let pipeline = match gst::parse::launch(&desc) {
            Ok(e) => e.downcast::<gst::Pipeline>().unwrap(),
            Err(e) => {
                eprintln!("audio: relay pipeline: {e}");
                AUDIO_CAPTURE.store(false, Ordering::SeqCst);
                return;
            }
        };
        let asink = pipeline
            .by_name("asink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        if pipeline.set_state(gst::State::Playing).is_err() {
            eprintln!("audio: relay would not start");
            AUDIO_CAPTURE.store(false, Ordering::SeqCst);
            return;
        }
        eprintln!("audio: relay listening on udp/{port}");
        loop {
            match asink.try_pull_sample(gst::ClockTime::from_seconds(1)) {
                Some(sample) => {
                    if let Some(map) = sample.buffer().and_then(|b| b.map_readable().ok()) {
                        if let Ok(mut q) = audio_pcm().lock() {
                            q.push_back(map.as_slice().to_vec());
                            while q.len() > 64 {
                                q.pop_front();
                            }
                        }
                    }
                }
                None if asink.is_eos() => break,
                None => {}
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        AUDIO_CAPTURE.store(false, Ordering::SeqCst);
    });
}

// Feeds an rtsp media's audiosrc: one 20 ms chunk (or silence) every 20 ms
fn run_audio_output(appsrc: gst_app::AppSrc, port: u16, shutdown: Arc<AtomicBool>) {
    ensure_audio_capture(port);
    appsrc.set_caps(Some(&audio_caps()));
    while !shutdown.load(Ordering::Relaxed) {
        if appsrc
            .push_buffer(gst::Buffer::from_slice(next_audio_chunk()))
            .is_err()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// A persistent pipeline: building one per frame would cost more than the encode
struct JpegEncoder {
    appsrc: gst_app::AppSrc,
    sink: gst_app::AppSink,
    pipeline: gst::Pipeline,
    caps_set: bool,
}

impl JpegEncoder {
    fn new() -> Result<Self, String> {
        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder().is_live(true).format(gst::Format::Time).build();
        let convert = gst::ElementFactory::make("videoconvert").build().map_err(|e| e.to_string())?;
        let enc = gst::ElementFactory::make("jpegenc").build().map_err(|e| e.to_string())?;
        let sink = gst_app::AppSink::builder().sync(false).max_buffers(1).drop(true).build();
        pipeline.add_many([appsrc.upcast_ref(), &convert, &enc, sink.upcast_ref()])
            .map_err(|e| e.to_string())?;
        gst::Element::link_many([appsrc.upcast_ref(), &convert, &enc, sink.upcast_ref()])
            .map_err(|e| e.to_string())?;
        pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;

        Ok(JpegEncoder { appsrc, sink, pipeline, caps_set: false })
    }

    fn encode(&mut self, frame: &LatestFrame) -> Option<Vec<u8>> {
        if !self.caps_set {
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", frame.gst_format)
                .field("width", frame.width as i32)
                .field("height", frame.height as i32)
                .field("framerate", gst::Fraction::new(framerate() as i32, 1))
                .build();
            self.appsrc.set_caps(Some(&caps));
            self.caps_set = true;
        }
        let buf = gst::Buffer::from_slice(frame.pixels.clone());
        self.appsrc.push_buffer(buf).ok()?;
        let sample = self.sink.try_pull_sample(gst::ClockTime::from_seconds(2))?;
        let map = sample.buffer()?.map_readable().ok()?;

        Some(map.as_slice().to_vec())
    }
}

impl Drop for JpegEncoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn serve_http(listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        std::thread::spawn(move || {
            if let Err(e) = handle_http(stream) {
                log_http(&format!("client gone: {e}"));
            }
        });
    }
}

fn log_http(msg: &str) {
    eprintln!("http: {msg}");
}

fn jpeg_encoder(stream: &mut TcpStream) -> std::io::Result<Option<JpegEncoder>> {
    match JpegEncoder::new() {
        Ok(encoder) => Ok(Some(encoder)),
        Err(e) => {
            log_http(&format!("no jpeg encoder: {e}"));
            stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")?;
            Ok(None)
        }
    }
}

fn handle_http(mut stream: TcpStream) -> std::io::Result<()> {
    let request = match Request::parse(&mut std::io::BufReader::new(&stream)) {
        Some(r) => r,
        None => {
            stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
    };

    let path = request.path().to_string();

    // The dlna endpoints answer descriptions and SOAP, not video: they must
    // not spin up capture or an encoder
    if dlna::handle_request(&mut stream, &request.method, &path,
                            &request.headers, &request.body, dlna_name())? {
        return Ok(());
    }

    if request.method != "GET" {
        stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")?;
        return Ok(());
    }

    // Answered without capture or an encoder, so a scrape never wakes the
    // pipeline
    if path == "/metrics" {
        let body = metrics::metrics_body();
        stream.write_all(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()).as_bytes())?;
        stream.write_all(body.as_bytes())?;
        return Ok(());
    }

    ensure_capture();
    match path.as_str() {
        "/snapshot" | "/screenshot" => {
            SNAPSHOTS_TOTAL.fetch_add(1, Ordering::Relaxed);
            let _guard = ClientGuard::new(&CLIENTS_SNAPSHOT);
            let Some(mut encoder) = jpeg_encoder(&mut stream)? else {
                return Ok(());
            };
            // One frame, for a save button or anything expecting a still
            for _ in 0..40 {
                if let Some(jpeg) = latest_jpeg(&mut encoder) {
                    stream.write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nContent-Length: {}\r\n\r\n",
                        jpeg.len()).as_bytes())?;
                    stream.write_all(&jpeg)?;
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")?;
        }
        "/" | "/webm" => {
            let _guard = ClientGuard::new(&CLIENTS_WEBM);
            match StreamEncoder::webm(audio_port()) {
                Ok(encoder) => stream_chunked(&mut stream, encoder,
                                              "video/webm")?,
                Err(e) => {
                    log_http(&format!("no webm encoder: {e}"));
                    stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")?;
                }
            }
        }
        // h264 in matroska is what the shipped plugins can produce for a
        // dlna player like the ps4, which plays neither vp8 nor rtsp
        "/stream.mkv" => {
            let _guard = ClientGuard::new(&CLIENTS_MKV);
            match StreamEncoder::matroska_h264() {
                Ok(encoder) => stream_chunked(&mut stream, encoder,
                                              "video/x-matroska")?,
                Err(e) => {
                    log_http(&format!("no matroska encoder: {e}"));
                    stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")?;
                }
            }
        }
        "/mjpeg" => {
            let _guard = ClientGuard::new(&CLIENTS_MJPEG);
            let Some(mut encoder) = jpeg_encoder(&mut stream)? else {
                return Ok(());
            };
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=screamframe\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
            loop {
                match latest_jpeg(&mut encoder) {
                    Some(jpeg) => {
                        stream.write_all(format!(
                            "--screamframe\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", jpeg.len()).as_bytes())?;
                        stream.write_all(&jpeg)?;
                        stream.write_all(b"\r\n")?;
                    }
                    None => std::thread::sleep(Duration::from_millis(100)),
                }
                std::thread::sleep(frame_interval());
            }
        }
        _ => {
            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
        }
    }

    Ok(())
}

// Chunked, so the response never ends and the client keeps playing what
// arrives. Content-Length would be a lie here
fn stream_chunked(stream: &mut TcpStream, mut encoder: StreamEncoder,
                  content_type: &str) -> std::io::Result<()> {
    stream.write_all(format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes())?;

    let mut idle = 0u32;
    // Audio chunks are 20 ms; a video frame interval is longer, so carry the
    // remainder and push as many chunks as fit each turn
    let mut audio_debt = Duration::ZERO;
    loop {
        let frame = frames().lock().ok().and_then(|f| f.clone());
        match frame {
            Some(f) => {
                idle = 0;
                if !encoder.push(&f) {
                    log_http("appsrc rejected a buffer, closing");
                    break;
                }
            }
            None => {
                idle += 1;
                if idle > 200 {
                    log_http("no frames to stream, closing");
                    break;
                }
            }
        }
        audio_debt += frame_interval();
        while audio_debt >= Duration::from_millis(20) {
            encoder.push_audio();
            audio_debt -= Duration::from_millis(20);
        }
        while let Some(chunk) = encoder.pull() {
            write!(stream, "{:x}\r\n", chunk.len())?;
            stream.write_all(&chunk)?;
            stream.write_all(b"\r\n")?;
            stream.flush()?;
        }
        std::thread::sleep(frame_interval());
    }
    let _ = stream.write_all(b"0\r\n\r\n");

    Ok(())
}

// One encode-and-mux pipeline per http client. vp8 in webm is what a browser
// <video> plays over a plain chunked http response: no MSE, no signalling and
// no javascript, unlike hls which desktop chrome and firefox will not touch
// without hls.js. h264 in matroska is the same shape for dlna players
struct StreamEncoder {
    appsrc: gst_app::AppSrc,
    audio: Option<gst_app::AppSrc>,
    sink: gst_app::AppSink,
    pipeline: gst::Pipeline,
    caps_set: bool,
}

impl StreamEncoder {
    // vp8 + opus in webm: the browser <video> plays both, and opus is what
    // scream already muxes for rtsp pay1
    fn webm(audio_port: u16) -> Result<Self, String> {
        // deadline=1 is vp8's realtime mode; without it the encoder happily
        // spends far longer than a frame interval on a frame
        let enc = gst::ElementFactory::make("vp8enc")
            .property("deadline", 1i64)
            .property("cpu-used", 8i32)
            .property("threads", 2i32)
            .build().map_err(|e| e.to_string())?;
        let mux = gst::ElementFactory::make("webmmux")
            .property("streamable", true)
            .build().map_err(|e| e.to_string())?;

        Self::build(enc, mux, audio_port)
    }

    fn matroska_h264() -> Result<Self, String> {
        // The same encode the rtsp stream runs; a short keyframe interval so
        // a player joining the live stream starts within a couple of seconds.
        // Video only: dlna players are fussy about opus in matroska
        let enc = gst::ElementFactory::make("x264enc")
            .property_from_str("speed-preset", "ultrafast")
            .property_from_str("tune", "zerolatency")
            .property("key-int-max", 60u32)
            .build().map_err(|e| e.to_string())?;
        let mux = gst::ElementFactory::make("matroskamux")
            .property("streamable", true)
            .build().map_err(|e| e.to_string())?;

        Self::build(enc, mux, 0)
    }

    fn build(enc: gst::Element, mux: gst::Element, audio_port: u16) -> Result<Self, String> {
        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder()
            .is_live(true).do_timestamp(true).format(gst::Format::Time).build();
        let convert = gst::ElementFactory::make("videoconvert").build().map_err(|e| e.to_string())?;
        let sink = gst_app::AppSink::builder().sync(false).max_buffers(4).drop(false).build();
        pipeline.add_many([appsrc.upcast_ref(), &convert, &enc, &mux, sink.upcast_ref()])
            .map_err(|e| e.to_string())?;
        gst::Element::link_many([appsrc.upcast_ref(), &convert, &enc])
            .map_err(|e| e.to_string())?;
        enc.link(&mux).map_err(|e| e.to_string())?;
        mux.link(&sink).map_err(|e| e.to_string())?;

        let audio = if audio_port != 0 {
            ensure_audio_capture(audio_port);
            let asrc = gst_app::AppSrc::builder()
                .is_live(true).do_timestamp(true).format(gst::Format::Time)
                .caps(&audio_caps()).build();
            let aconv = gst::ElementFactory::make("audioconvert").build().map_err(|e| e.to_string())?;
            let ares = gst::ElementFactory::make("audioresample").build().map_err(|e| e.to_string())?;
            let aenc = gst::ElementFactory::make("opusenc")
                .property("bitrate", 96_000i32).build().map_err(|e| e.to_string())?;
            pipeline.add_many([asrc.upcast_ref(), &aconv, &ares, &aenc])
                .map_err(|e| e.to_string())?;
            gst::Element::link_many([asrc.upcast_ref(), &aconv, &ares, &aenc])
                .map_err(|e| e.to_string())?;
            aenc.link(&mux).map_err(|e| e.to_string())?;
            Some(asrc)
        } else {
            None
        };

        pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;

        Ok(StreamEncoder { appsrc, audio, sink, pipeline, caps_set: false })
    }

    fn push(&mut self, frame: &LatestFrame) -> bool {
        if !self.caps_set {
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", frame.gst_format)
                .field("width", frame.width as i32)
                .field("height", frame.height as i32)
                .field("framerate", gst::Fraction::new(framerate() as i32, 1))
                .build();
            self.appsrc.set_caps(Some(&caps));
            self.caps_set = true;
        }

        self.appsrc.push_buffer(gst::Buffer::from_slice(frame.pixels.clone())).is_ok()
    }

    fn push_audio(&mut self) {
        if let Some(src) = &self.audio {
            let _ = src.push_buffer(gst::Buffer::from_slice(next_audio_chunk()));
        }
    }

    fn pull(&mut self) -> Option<Vec<u8>> {
        let sample = self.sink.try_pull_sample(gst::ClockTime::from_mseconds(50))?;
        let map = sample.buffer()?.map_readable().ok()?;

        Some(map.as_slice().to_vec())
    }
}

impl Drop for StreamEncoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn latest_jpeg(encoder: &mut JpegEncoder) -> Option<Vec<u8>> {
    let frame = frames().lock().ok()?.clone()?;

    encoder.encode(&frame)
}

// Coordinator — monitors toplevels + wl_output, fires events

enum CoordEvent {
    NewToplevel { identifier: String, app_id: String, title: String },
    ToplevelClosed { identifier: String },
    OutputSize { width: u32, height: u32 },
}

struct CoordTop {
    handle: ExtForeignToplevelHandleV1,
    identifier: String,
    app_id: String,
    title: String,
    done: bool,
    emitted: bool,
}

#[derive(Default)]
struct CoordState {
    toplevels: Vec<CoordTop>,
    closed_identifiers: Vec<String>,
    output_w: u32,
    output_h: u32,
}

impl Dispatch<WlRegistry, ()> for CoordState {
    fn event(
        state: &mut Self, registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &(), _: &Connection, qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_output" => {
                    if state.output_w == 0 {
                        registry.bind::<WlOutput, _, _>(name, version.min(2), qh, ());
                    }
                }
                "ext_foreign_toplevel_list_v1" => {
                    registry.bind::<ExtForeignToplevelListV1, _, _>(name, version.min(1), qh, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for CoordState {
    fn event(
        state: &mut Self, _: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { width, height, .. } = event {
            state.output_w = width as u32;
            state.output_h = height as u32;
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for CoordState {
    fn event(
        state: &mut Self, _: &ExtForeignToplevelListV1,
        event: <ExtForeignToplevelListV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(CoordTop {
                handle: toplevel,
                identifier: String::new(),
                app_id: String::new(),
                title: String::new(),
                done: false,
                emitted: false,
            });
        }
    }
    wayland_client::event_created_child!(CoordState, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ())
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for CoordState {
    fn event(
        state: &mut Self, proxy: &ExtForeignToplevelHandleV1,
        event: <ExtForeignToplevelHandleV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let Some(t) = state.toplevels.iter_mut().find(|t| &t.handle == proxy) {
            match event {
                ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => t.identifier = identifier,
                ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => t.app_id = app_id,
                ext_foreign_toplevel_handle_v1::Event::Title { title } => t.title = title,
                ext_foreign_toplevel_handle_v1::Event::Done => t.done = true,
                ext_foreign_toplevel_handle_v1::Event::Closed => {
                    // Save identifier before dropping the entry so we can emit ToplevelClosed
                    let id = state.toplevels.iter()
                        .find(|t| &t.handle == proxy)
                        .map(|t| t.identifier.clone())
                        .unwrap_or_default();
                    if !id.is_empty() {
                        state.closed_identifiers.push(id);
                    }
                    state.toplevels.retain(|t| &t.handle != proxy);
                }
                _ => {}
            }
        }
    }
}

// Long-running coordinator: sends CoordEvents as toplevels or the output change
fn coordinator_loop(tx: mpsc::Sender<CoordEvent>, shutdown: Arc<AtomicBool>) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => { eprintln!("coordinator: Wayland connect failed: {e}"); return; }
    };
    let mut eq: EventQueue<CoordState> = conn.new_event_queue();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());
    let mut state = CoordState::default();

    // Two roundtrips: first to collect globals, second for initial toplevel properties
    if eq.roundtrip(&mut state).is_err() { return; }
    if eq.roundtrip(&mut state).is_err() { return; }

    let mut last_output_w = 0u32;
    let mut last_output_h = 0u32;

    while !shutdown.load(Ordering::Relaxed) {
        if eq.dispatch_pending(&mut state).is_err() {
            eprintln!("coordinator: compositor disconnected");
            break;
        }
        if conn.flush().is_err() {
            eprintln!("coordinator: flush failed, compositor may have gone away");
            break;
        }

        // Emit OutputSize when wl_output dimensions change
        if (state.output_w != last_output_w || state.output_h != last_output_h)
            && state.output_w > 0 && state.output_h > 0
        {
            last_output_w = state.output_w;
            last_output_h = state.output_h;
            let _ = tx.send(CoordEvent::OutputSize { width: state.output_w, height: state.output_h });
        }

        // Emit NewToplevel for each fully-initialized, not-yet-emitted handle
        for t in state.toplevels.iter_mut().filter(|t| t.done && !t.emitted && !t.identifier.is_empty()) {
            let _ = tx.send(CoordEvent::NewToplevel {
                identifier: t.identifier.clone(),
                app_id: t.app_id.clone(),
                title: t.title.clone(),
            });
            t.emitted = true;
        }

        // Emit ToplevelClosed for handles that reported Closed
        for id in state.closed_identifiers.drain(..) {
            let _ = tx.send(CoordEvent::ToplevelClosed { identifier: id });
        }

        std::thread::sleep(Duration::from_millis(16));
    }
}

// Output-size query — blocking, one-shot

#[derive(Default)]
struct SizeQueryState {
    width: u32,
    height: u32,
}

impl Dispatch<WlRegistry, ()> for SizeQueryState {
    fn event(
        _: &mut Self, registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &(), _: &Connection, qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == "wl_output" {
                registry.bind::<WlOutput, _, _>(name, version.min(2), qh, ());
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for SizeQueryState {
    fn event(
        state: &mut Self, _: &WlOutput,
        event: <WlOutput as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { width, height, .. } = event {
            state.width = width as u32;
            state.height = height as u32;
        }
    }
}

fn query_output_size() -> Option<(u32, u32)> {
    let conn = Connection::connect_to_env().ok()?;
    let mut eq: EventQueue<SizeQueryState> = conn.new_event_queue();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());
    let mut state = SizeQueryState::default();
    eq.roundtrip(&mut state).ok()?;
    eq.roundtrip(&mut state).ok()?;
    if state.width > 0 && state.height > 0 { Some((state.width, state.height)) } else { None }
}

// Capture-thread Wayland state
//
// Each capture thread gets its own Wayland connection and binds
// ext_foreign_toplevel_list_v1 to find its target handle by identifier, then
// uses ext_foreign_toplevel_image_capture_source_manager_v1 to capture frames

struct CaptureState {
    shm: Option<WlShm>,
    toplevel_list: Option<ExtForeignToplevelListV1>,
    toplevel_source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    toplevels: Vec<CapTop>,
    width: u32,
    height: u32,
    shm_format: Option<wl_shm::Format>,
    constraints_dirty: bool,
    session_stopped: bool,
    frame_result: Option<FrameResult>,
}

impl Default for CaptureState {
    fn default() -> Self {
        Self {
            shm: None, toplevel_list: None,
            toplevel_source_manager: None, capture_manager: None,
            toplevels: Vec::new(),
            width: 0, height: 0, shm_format: None,
            constraints_dirty: false, session_stopped: false, frame_result: None,
        }
    }
}

struct CapTop {
    handle: ExtForeignToplevelHandleV1,
    identifier: String,
    done: bool,
}

enum FrameResult {
    Ready,
    Failed(WEnum<ext_image_copy_capture_frame_v1::FailureReason>),
}

impl Dispatch<WlRegistry, ()> for CaptureState {
    fn event(
        state: &mut Self, registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &(), _: &Connection, qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "ext_foreign_toplevel_list_v1" =>
                    state.toplevel_list = Some(registry.bind(name, version.min(1), qh, ())),
                "ext_foreign_toplevel_image_capture_source_manager_v1" =>
                    state.toplevel_source_manager = Some(registry.bind(name, version.min(1), qh, ())),
                "ext_image_copy_capture_manager_v1" =>
                    state.capture_manager = Some(registry.bind(name, version.min(1), qh, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<WlShm, ()> for CaptureState {
    fn event(_: &mut Self, _: &WlShm, _: <WlShm as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShmPool, ()> for CaptureState {
    fn event(_: &mut Self, _: &WlShmPool, _: <WlShmPool as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlBuffer, ()> for CaptureState {
    fn event(_: &mut Self, _: &WlBuffer, _: <WlBuffer as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtForeignToplevelListV1, ()> for CaptureState {
    fn event(
        state: &mut Self, _: &ExtForeignToplevelListV1,
        event: <ExtForeignToplevelListV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(CapTop { handle: toplevel, identifier: String::new(), done: false });
        }
    }
    wayland_client::event_created_child!(CaptureState, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ())
    ]);
}
impl Dispatch<ExtForeignToplevelHandleV1, ()> for CaptureState {
    fn event(
        state: &mut Self, proxy: &ExtForeignToplevelHandleV1,
        event: <ExtForeignToplevelHandleV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let Some(t) = state.toplevels.iter_mut().find(|t| &t.handle == proxy) {
            match event {
                ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => t.identifier = identifier,
                ext_foreign_toplevel_handle_v1::Event::Done => t.done = true,
                // Window closed while we were still waiting; treat as session_stopped
                ext_foreign_toplevel_handle_v1::Event::Closed => {
                    state.session_stopped = true;
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for CaptureState {
    fn event(_: &mut Self, _: &ExtForeignToplevelImageCaptureSourceManagerV1, _: <ExtForeignToplevelImageCaptureSourceManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtImageCaptureSourceV1, ()> for CaptureState {
    fn event(_: &mut Self, _: &ExtImageCaptureSourceV1, _: <ExtImageCaptureSourceV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for CaptureState {
    fn event(_: &mut Self, _: &ExtImageCopyCaptureManagerV1, _: <ExtImageCopyCaptureManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for CaptureState {
    fn event(
        state: &mut Self, _: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => { state.width = width; state.height = height; }
            Event::ShmFormat { format: WEnum::Value(f) }
                if state.shm_format.is_none() && matches!(f, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888) =>
            { state.shm_format = Some(f); }
            Event::Done => state.constraints_dirty = true,
            Event::Stopped => state.session_stopped = true,
            _ => {}
        }
    }
}
impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self, _: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => state.frame_result = Some(FrameResult::Ready),
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => state.frame_result = Some(FrameResult::Failed(reason)),
            _ => {}
        }
    }
}

// ShmBuffer

struct ShmBuffer {
    _file: std::fs::File,
    mmap: memmap2::MmapMut,
    pool: WlShmPool,
    buffer: WlBuffer,
    width: u32,
    height: u32,
    stride: i32,
}

fn new_shm_buffer<S>(
    shm: &WlShm, qh: &QueueHandle<S>,
    width: u32, height: u32, format: wl_shm::Format,
) -> ShmBuffer
where
    S: Dispatch<WlShmPool, ()> + Dispatch<WlBuffer, ()> + 'static,
{
    let stride = width as i32 * 4;
    let size = stride as i64 * height as i64;
    let file = tempfile::tempfile().expect("shm tempfile");
    file.set_len(size as u64).expect("size shm");
    let mmap = unsafe { memmap2::MmapMut::map_mut(&file).expect("mmap") };
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride, format, qh, ());
    ShmBuffer { _file: file, mmap, pool, buffer, width, height, stride }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) { self.buffer.destroy(); self.pool.destroy(); }
}

fn gst_video_format(format: wl_shm::Format) -> Option<&'static str> {
    match format {
        wl_shm::Format::Argb8888 => Some("BGRA"),
        wl_shm::Format::Xrgb8888 => Some("BGRx"),
        _ => None,
    }
}

// Toplevel capture thread
//
// Connects to the compositor independently, finds the target toplevel by its
// stable identifier string, then runs the frame loop until the session stops
// (window closed, compositor gone) or shutdown is signalled

fn wayland_capture_loop_toplevel(
    target_id: String,
    slot: FrameSlot,
    shutdown: Arc<AtomicBool>,
) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => { eprintln!("capture[{target_id}]: connect: {e}"); return; }
    };
    let mut eq: EventQueue<CaptureState> = conn.new_event_queue();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());
    let mut state = CaptureState::default();
    if eq.roundtrip(&mut state).is_err() { return; }

    let (Some(shm), Some(src_mgr), Some(cap_mgr)) =
        (state.shm.take(), state.toplevel_source_manager.take(), state.capture_manager.take())
    else {
        eprintln!("capture[{target_id}]: missing globals");
        return;
    };

    // Collect the initial toplevel list; wait for Done events
    if eq.roundtrip(&mut state).is_err() { return; }
    if eq.roundtrip(&mut state).is_err() { return; }

    let handle = state.toplevels.iter()
        .find(|t| t.identifier == target_id && t.done)
        .map(|t| t.handle.clone());

    // Window might arrive slightly after the initial batch if it was just opened
    let handle = if let Some(h) = handle { h } else {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if shutdown.load(Ordering::Relaxed) || state.session_stopped { return; }
            if eq.dispatch_pending(&mut state).is_err() { return; }
            if conn.flush().is_err() { return; }
            if let Some(h) = state.toplevels.iter().find(|t| t.identifier == target_id && t.done) {
                break h.handle.clone();
            }
            if std::time::Instant::now() > deadline {
                eprintln!("capture[{target_id}]: toplevel not found");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let source = src_mgr.create_source(&handle, &qh, ());
    let session = cap_mgr.create_session(&source, Options::empty(), &qh, ());

    while !state.constraints_dirty && !state.session_stopped && !shutdown.load(Ordering::Relaxed) {
        if eq.blocking_dispatch(&mut state).is_err() { return; }
    }
    if shutdown.load(Ordering::Relaxed) { return; }

    let mut shm_buf: Option<ShmBuffer> = None;
    let mut gst_format: &'static str = "";

    while !shutdown.load(Ordering::Relaxed) {
        if state.session_stopped { return; }

        if state.constraints_dirty {
            state.constraints_dirty = false;
            let Some(fmt) = state.shm_format.take() else { return; };
            const MAX: u32 = 16384;
            if state.width == 0 || state.height == 0 || state.width > MAX || state.height > MAX { return; }
            let needs = shm_buf.as_ref().is_none_or(|b| b.width != state.width || b.height != state.height);
            if needs { shm_buf = Some(new_shm_buffer(&shm, &qh, state.width, state.height, fmt)); }
            let Some(gf) = gst_video_format(fmt) else { return; };
            gst_format = gf;
        }

        let Some(buf) = shm_buf.as_ref() else {
            if eq.blocking_dispatch(&mut state).is_err() { return; }
            continue;
        };

        let frame = session.create_frame(&qh, ());
        frame.attach_buffer(&buf.buffer);
        frame.damage_buffer(0, 0, buf.width as i32, buf.height as i32);
        frame.capture();

        state.frame_result = None;
        while state.frame_result.is_none() && !state.session_stopped && !shutdown.load(Ordering::Relaxed) {
            if eq.blocking_dispatch(&mut state).is_err() { frame.destroy(); return; }
        }
        frame.destroy();

        match state.frame_result.take() {
            Some(FrameResult::Ready) => {
                let pixels = buf.mmap[..(buf.stride as usize * buf.height as usize)].to_vec();
                *slot.lock().unwrap() = Some(LatestFrame { pixels, width: buf.width, height: buf.height, gst_format });
            }
            Some(FrameResult::Failed(r)) => {
                eprintln!("capture[{target_id}]: frame failed: {r:?}");
                std::thread::sleep(Duration::from_millis(100));
            }
            None => return,
        }
    }
}

// GStreamer compositor pipeline

struct PipelineStream {
    identifier: String,
    slot: FrameSlot,
    appsrc: gst_app::AppSrc,
    chain: Vec<gst::Element>, // videoconvert, videoscale, capsfilter, queue
    comp_sink: gst::Pad,
    caps_set: bool,
    capture_shutdown: Arc<AtomicBool>,
}

struct CompositorPipeline {
    pipeline: gst::Pipeline,
    compositor: gst::Element,
    out_caps_filter: gst::Element,
    streams: Vec<PipelineStream>,
    out_w: u32,
    out_h: u32,
}

impl CompositorPipeline {
    // Build the skeleton pipeline with a test-card background source and the fixed
    // downstream chain; window streams are added dynamically via add_stream
    fn new(out_w: u32, out_h: u32) -> Self {
        let pipeline = gst::Pipeline::new();

        // SMPTE test card ensures the compositor always has data to output,
        // even before any window streams are attached; also useful for
        // verifying the RTSP stream is alive before any toplevels appear
        let bg_src = gst::ElementFactory::make("videotestsrc")
            .property_from_str("pattern", "smpte")
            .property("is-live", true)
            .build().expect("videotestsrc");
        let bg_caps_filter = gst::ElementFactory::make("capsfilter").build().expect("capsfilter");
        bg_caps_filter.set_property("caps", &gst::Caps::builder("video/x-raw")
            .field("format", "BGRx")
            .field("width", out_w as i32)
            .field("height", out_h as i32)
            .field("framerate", gst::Fraction::new(framerate() as i32, 1))
            .build());

        let compositor = gst::ElementFactory::make("compositor").build().expect("compositor");
        let out_caps_filter = gst::ElementFactory::make("capsfilter").build().expect("capsfilter");
        out_caps_filter.set_property("caps", &gst::Caps::builder("video/x-raw")
            .field("width", out_w as i32)
            .field("height", out_h as i32)
            .build());
        let post_convert = gst::ElementFactory::make("videoconvert").build().expect("videoconvert");
        let encoder = gst::ElementFactory::make("x264enc")
            .property_from_str("speed-preset", "ultrafast")
            .property_from_str("tune", "zerolatency")
            .property("key-int-max", framerate())
            .build().expect("x264enc");
        // Buffering ahead of the encoder and ahead of the rtsp server's appsink
        // so neither hits its processing deadline with nothing queued
        let bg_queue = make_queue();
        let out_queue = make_queue();
        let pay_queue = make_queue();
        let pay = gst::ElementFactory::make("rtph264pay")
            .name("pay0")
            .property("pt", 96u32)
            .property("config-interval", -1i32)
            .build().expect("rtph264pay");

        pipeline.add_many([&bg_src, &bg_caps_filter, &bg_queue, &compositor,
                           &out_caps_filter, &out_queue, &post_convert,
                           &encoder, &pay_queue, &pay]).unwrap();

        gst::Element::link_many([&bg_src, &bg_caps_filter, &bg_queue]).unwrap();
        let bg_sink = compositor.request_pad_simple("sink_%u").expect("compositor bg sink");
        bg_sink.set_property("zorder", 0i32);
        bg_sink.set_property("xpos", 0i32);
        bg_sink.set_property("ypos", 0i32);
        bg_sink.set_property("width", out_w as i32);
        bg_sink.set_property("height", out_h as i32);
        bg_queue.static_pad("src").unwrap().link(&bg_sink).unwrap();

        gst::Element::link_many([&compositor, &out_caps_filter, &out_queue,
                                 &post_convert, &encoder, &pay_queue, &pay]).unwrap();

        CompositorPipeline { pipeline, compositor, out_caps_filter, streams: Vec::new(), out_w, out_h }
    }

    fn add_stream(&mut self, identifier: String, slot: FrameSlot, capture_shutdown: Arc<AtomicBool>) {
        let n = self.streams.len() + 1; // account for the background occupying sink_0
        let Grid { cell_w, cell_h, .. } = Grid::new(n, self.out_w, self.out_h);

        let placeholder_caps = gst::Caps::builder("video/x-raw")
            .field("format", "BGRx").field("width", 16i32).field("height", 16i32)
            .field("framerate", gst::Fraction::new(framerate() as i32, 1)).build();
        let appsrc = gst_app::AppSrc::builder()
            .is_live(true).do_timestamp(true).format(gst::Format::Time)
            .caps(&placeholder_caps).build();

        let convert = gst::ElementFactory::make("videoconvert").build().expect("videoconvert");
        let scale   = gst::ElementFactory::make("videoscale").build().expect("videoscale");
        let cf      = gst::ElementFactory::make("capsfilter").build().expect("capsfilter");
        cf.set_property("caps", &gst::Caps::builder("video/x-raw")
            .field("width", cell_w as i32).field("height", cell_h as i32).build());
        let queue = make_queue();

        self.pipeline.add_many([appsrc.upcast_ref(), &convert, &scale, &cf, &queue]).unwrap();
        gst::Element::link_many([appsrc.upcast_ref(), &convert, &scale, &cf, &queue]).unwrap();

        let comp_sink = self.compositor.request_pad_simple("sink_%u").expect("compositor sink");
        comp_sink.set_property("zorder", 1i32);
        queue.static_pad("src").unwrap().link(&comp_sink).unwrap();

        for el in [appsrc.upcast_ref(), &convert, &scale, &cf, &queue] {
            el.sync_state_with_parent().ok();
        }

        self.streams.push(PipelineStream {
            identifier, slot, appsrc,
            chain: vec![convert, scale, cf, queue],
            comp_sink, caps_set: false, capture_shutdown,
        });

        self.recalculate_layout();
    }

    fn remove_stream(&mut self, identifier: &str) {
        let Some(pos) = self.streams.iter().position(|s| s.identifier == identifier) else { return; };
        let stream = self.streams.remove(pos);

        // Signal capture thread to exit
        stream.capture_shutdown.store(true, Ordering::Relaxed);

        // Unlink capsfilter src from compositor sink, then release the pad
        if let Some(src) = stream.chain.last().and_then(|el| el.static_pad("src")) {
            let _ = src.unlink(&stream.comp_sink);
        }
        self.compositor.release_request_pad(&stream.comp_sink);

        // Tear down the chain in reverse so nothing flows while unlinking
        let _ = stream.appsrc.set_state(gst::State::Null);
        self.pipeline.remove(&stream.appsrc).ok();
        for el in stream.chain.iter().rev() {
            let _ = el.set_state(gst::State::Null);
            self.pipeline.remove(el).ok();
        }

        self.recalculate_layout();
    }

    fn recalculate_layout(&self) {
        let n = self.streams.len();
        if n == 0 { return; }
        let grid = Grid::new(n, self.out_w, self.out_h);
        for (i, s) in self.streams.iter().enumerate() {
            let (xpos, ypos) = grid.cell_origin(i);
            s.comp_sink.set_property("xpos",   xpos as i32);
            s.comp_sink.set_property("ypos",   ypos as i32);
            s.comp_sink.set_property("width",  grid.cell_w as i32);
            s.comp_sink.set_property("height", grid.cell_h as i32);
        }
    }

    fn update_output_size(&mut self, w: u32, h: u32) {
        if self.out_w == w && self.out_h == h { return; }
        self.out_w = w;
        self.out_h = h;
        self.out_caps_filter.set_property("caps", &gst::Caps::builder("video/x-raw")
            .field("width", w as i32).field("height", h as i32).build());
        self.recalculate_layout();
    }
}

// Toplevel-mode orchestration

type SharedPipeline = Arc<Mutex<CompositorPipeline>>;

// Pipeline manager — receives coordinator events and mutates the live pipeline
fn pipeline_manager(
    shared: SharedPipeline,
    rx: mpsc::Receiver<CoordEvent>,
    dynamic_size: bool,
    shutdown: Arc<AtomicBool>,
) {
    for event in rx {
        if shutdown.load(Ordering::Relaxed) { break; }
        match event {
            CoordEvent::OutputSize { width, height } => {
                if dynamic_size {
                    shared.lock().unwrap().update_output_size(width, height);
                }
            }
            CoordEvent::NewToplevel { identifier, app_id, title } => {
                let mut pl = shared.lock().unwrap();
                if pl.streams.iter().any(|s| s.identifier == identifier) {
                    continue;
                }
                eprintln!("toplevel: capturing «{app_id}» — {title}");
                let slot: FrameSlot = Arc::new(Mutex::new(None));
                let cap_sd = Arc::new(AtomicBool::new(false));
                {
                    let slot2 = slot.clone();
                    let sd2   = cap_sd.clone();
                    let id2   = identifier.clone();
                    let overall = shutdown.clone();
                    std::thread::spawn(move || {
                        // Forward whichever of the two shutdown signals fires first
                        let combined = Arc::new(AtomicBool::new(false));
                        let comb2    = combined.clone();
                        std::thread::spawn(move || {
                            while !sd2.load(Ordering::Relaxed) && !overall.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                            comb2.store(true, Ordering::Relaxed);
                        });
                        wayland_capture_loop_toplevel(id2, slot2, combined);
                    });
                }
                pl.add_stream(identifier, slot, cap_sd);
            }
            CoordEvent::ToplevelClosed { identifier } => {
                eprintln!("toplevel: removing closed window {identifier}");
                shared.lock().unwrap().remove_stream(&identifier);
            }
        }
    }
}

// GStreamer feed loop — pushes the latest captured frame for each stream at 30 fps
fn feed_loop(shared: SharedPipeline, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        {
            let mut pl = shared.lock().unwrap();
            for s in pl.streams.iter_mut() {
                if let Some(frame) = s.slot.lock().unwrap().clone() {
                    if !s.caps_set {
                        let caps = gst::Caps::builder("video/x-raw")
                            .field("format", frame.gst_format)
                            .field("width",  frame.width as i32)
                            .field("height", frame.height as i32)
                            .field("framerate", gst::Fraction::new(framerate() as i32, 1))
                            .build();
                        s.appsrc.set_caps(Some(&caps));
                        s.caps_set = true;
                    }
                    let buf = gst::Buffer::from_slice(frame.pixels);
                    let _ = s.appsrc.push_buffer(buf);
                }
            }
        }
        std::thread::sleep(frame_interval());
    }
}

fn run_capture_toplevel(
    media: &gstreamer_rtsp_server::RTSPMedia,
    out_w: u32,
    out_h: u32,
    dynamic_size: bool,
    shutdown: Arc<AtomicBool>,
) {
    let comp_pipeline = CompositorPipeline::new(out_w, out_h);
    let raw_pipeline  = comp_pipeline.pipeline.clone();
    let shared: SharedPipeline = Arc::new(Mutex::new(comp_pipeline));

    media.take_pipeline(&raw_pipeline);

    let (tx, rx) = mpsc::channel::<CoordEvent>();

    { let sd = shutdown.clone(); std::thread::spawn(move || coordinator_loop(tx, sd)); }
    { let sh = shared.clone(); let sd = shutdown.clone(); std::thread::spawn(move || pipeline_manager(sh, rx, dynamic_size, sd)); }
    { let sh = shared.clone(); let sd = shutdown.clone(); std::thread::spawn(move || feed_loop(sh, sd)); }
}

// Output-mode capture

#[derive(Default)]
struct OutputState {
    output: Option<WlOutput>,
    shm: Option<WlShm>,
    source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    width: u32,
    height: u32,
    shm_format: Option<wl_shm::Format>,
    constraints_dirty: bool,
    session_stopped: bool,
    frame_result: Option<FrameResult>,
}

impl Dispatch<WlRegistry, ()> for OutputState {
    fn event(
        state: &mut Self, registry: &WlRegistry,
        event: <WlRegistry as Proxy>::Event,
        _: &(), _: &Connection, qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_output" => {
                    if state.output.is_none() {
                        state.output = Some(registry.bind(name, version.min(4), qh, ()));
                    }
                }
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "ext_output_image_capture_source_manager_v1" =>
                    state.source_manager = Some(registry.bind(name, version.min(1), qh, ())),
                "ext_image_copy_capture_manager_v1" =>
                    state.capture_manager = Some(registry.bind(name, version.min(1), qh, ())),
                _ => {}
            }
        }
    }
}
impl Dispatch<WlOutput, ()> for OutputState {
    fn event(_: &mut Self, _: &WlOutput, _: <WlOutput as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShm, ()> for OutputState {
    fn event(_: &mut Self, _: &WlShm, _: <WlShm as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShmPool, ()> for OutputState {
    fn event(_: &mut Self, _: &WlShmPool, _: <WlShmPool as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlBuffer, ()> for OutputState {
    fn event(_: &mut Self, _: &WlBuffer, _: <WlBuffer as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for OutputState {
    fn event(_: &mut Self, _: &ExtOutputImageCaptureSourceManagerV1, _: <ExtOutputImageCaptureSourceManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtImageCaptureSourceV1, ()> for OutputState {
    fn event(_: &mut Self, _: &ExtImageCaptureSourceV1, _: <ExtImageCaptureSourceV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for OutputState {
    fn event(_: &mut Self, _: &ExtImageCopyCaptureManagerV1, _: <ExtImageCopyCaptureManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for OutputState {
    fn event(
        state: &mut Self, _: &ExtImageCopyCaptureSessionV1,
        event: <ExtImageCopyCaptureSessionV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => { state.width = width; state.height = height; }
            Event::ShmFormat { format: WEnum::Value(f) }
                if state.shm_format.is_none() && matches!(f, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888) =>
            { state.shm_format = Some(f); }
            Event::Done    => state.constraints_dirty = true,
            Event::Stopped => state.session_stopped = true,
            _ => {}
        }
    }
}
impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for OutputState {
    fn event(
        state: &mut Self, _: &ExtImageCopyCaptureFrameV1,
        event: <ExtImageCopyCaptureFrameV1 as Proxy>::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => state.frame_result = Some(FrameResult::Ready),
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => state.frame_result = Some(FrameResult::Failed(reason)),
            _ => {}
        }
    }
}

fn wayland_capture_loop_output(latest_frame: Arc<Mutex<Option<LatestFrame>>>, shutdown: Arc<AtomicBool>) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => { eprintln!("output capture: connect: {e}"); return; }
    };
    let mut eq: EventQueue<OutputState> = conn.new_event_queue();
    let qh = eq.handle();
    conn.display().get_registry(&qh, ());
    let mut state = OutputState::default();
    if eq.roundtrip(&mut state).is_err() { return; }

    let (Some(output), Some(shm), Some(src_mgr), Some(cap_mgr)) =
        (state.output.take(), state.shm.take(), state.source_manager.take(), state.capture_manager.take())
    else {
        eprintln!("output capture: missing globals");
        return;
    };

    let source  = src_mgr.create_source(&output, &qh, ());
    let session = cap_mgr.create_session(&source, Options::empty(), &qh, ());

    while !state.constraints_dirty && !state.session_stopped && !shutdown.load(Ordering::Relaxed) {
        if eq.blocking_dispatch(&mut state).is_err() { return; }
    }
    if shutdown.load(Ordering::Relaxed) { return; }

    let mut shm_buf: Option<ShmBuffer> = None;
    let mut gst_format: &'static str = "";

    while !shutdown.load(Ordering::Relaxed) {
        if state.session_stopped { return; }
        if state.constraints_dirty {
            state.constraints_dirty = false;
            let Some(fmt) = state.shm_format.take() else { return; };
            const MAX: u32 = 16384;
            if state.width == 0 || state.height == 0 || state.width > MAX || state.height > MAX { return; }
            let needs = shm_buf.as_ref().is_none_or(|b| b.width != state.width || b.height != state.height);
            if needs { shm_buf = Some(new_shm_buffer(&shm, &qh, state.width, state.height, fmt)); }
            let Some(gf) = gst_video_format(fmt) else { return; };
            gst_format = gf;
        }
        let Some(buf) = shm_buf.as_ref() else {
            if eq.blocking_dispatch(&mut state).is_err() { return; }
            continue;
        };

        let frame = session.create_frame(&qh, ());
        frame.attach_buffer(&buf.buffer);
        frame.damage_buffer(0, 0, buf.width as i32, buf.height as i32);
        frame.capture();

        state.frame_result = None;
        while state.frame_result.is_none() && !state.session_stopped && !shutdown.load(Ordering::Relaxed) {
            if eq.blocking_dispatch(&mut state).is_err() { frame.destroy(); return; }
        }
        frame.destroy();

        match state.frame_result.take() {
            Some(FrameResult::Ready) => {
                let pixels = buf.mmap[..(buf.stride as usize * buf.height as usize)].to_vec();
                *latest_frame.lock().unwrap() =
                    Some(LatestFrame { pixels, width: buf.width, height: buf.height, gst_format });
            }
            Some(FrameResult::Failed(r)) => {
                eprintln!("output frame failed: {r:?}");
                std::thread::sleep(Duration::from_millis(100));
            }
            None => return,
        }
    }
}

fn run_capture_output(appsrc: gst_app::AppSrc, shutdown: Arc<AtomicBool>) {
    ensure_capture();
    let latest_frame = frames().clone();
    let mut caps_set = false;
    while !shutdown.load(Ordering::Relaxed) {
        if let Some(frame) = latest_frame.lock().unwrap().clone() {
            if !caps_set {
                let caps = gst::Caps::builder("video/x-raw")
                    .field("format", frame.gst_format)
                    .field("width",  frame.width as i32)
                    .field("height", frame.height as i32)
                    .field("framerate", gst::Fraction::new(framerate() as i32, 1))
                    .build();
                appsrc.set_caps(Some(&caps));
                caps_set = true;
            }
            let buf = gst::Buffer::from_slice(frame.pixels);
            if appsrc.push_buffer(buf).is_err() { return; }
        }
        std::thread::sleep(frame_interval());
    }
}

fn main() {
    if std::env::var("GST_DEBUG").is_err() { std::env::set_var("GST_DEBUG", "2"); }
    gst::init().expect("GStreamer init");

    let args = Args::parse();
    FRAMERATE.store(args.framerate.max(1), Ordering::Relaxed);
    AUDIO_PORT.store(args.audio_port, Ordering::Relaxed);

    let server = RTSPServer::new();
    server.set_address(&args.bind_address);
    server.set_service(&args.bind_port);

    // One connected rtsp client to one open control connection; the count is
    // read back at /metrics
    server.connect_client_connected(|_, client| {
        CLIENTS_RTSP.fetch_add(1, Ordering::Relaxed);
        client.connect_closed(|_| {
            CLIENTS_RTSP.fetch_sub(1, Ordering::Relaxed);
        });
    });

    let factory = RTSPMediaFactory::new();
    factory.set_shared(true);

    match args.mode {
        Mode::Output => {
            // Queues on both sides of the encoder: the live appsrc and the
            // rtsp server's own appsink each need buffering to meet their
            // processing deadline, otherwise gst warns and runs at zero
            // latency, so a single slow frame arrives late
            // pay1 is a second stream in the same session: the media player's
            // relayed audio, re-encoded to opus. audiosrc always pushes, so
            // it is silence when no media view is up
            let audio_port = args.audio_port;
            let audio_branch = if audio_port != 0 {
                " appsrc name=audiosrc is-live=true do-timestamp=true format=time \
                  caps=audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
                  ! queue ! audioconvert ! audioresample \
                  ! opusenc bitrate=96000 \
                  ! queue ! rtpopuspay name=pay1 pt=97"
            } else {
                ""
            };
            factory.set_launch(&format!(
                "appsrc name=mysrc is-live=true do-timestamp=true format=time \
                 caps=video/x-raw,format=BGRx,width=16,height=16,framerate={fps}/1 \
                 ! queue leaky=downstream max-size-time=200000000 \
                   max-size-bytes=0 max-size-buffers=0 \
                 ! videoconvert ! video/x-raw,format=I420 \
                 ! x264enc speed-preset=ultrafast tune=zerolatency key-int-max={fps} \
                 ! queue max-size-time=200000000 max-size-bytes=0 max-size-buffers=0 \
                 ! rtph264pay name=pay0 pt=96 config-interval=1{audio}",
                fps = framerate(), audio = audio_branch
            ));
            factory.connect_media_configure(move |_, media| {
                let element = media.element();
                let bin     = element.downcast_ref::<gst::Bin>().unwrap();
                let appsrc  = bin.by_name("mysrc").unwrap().downcast::<gst_app::AppSrc>().unwrap();
                let shutdown = Arc::new(AtomicBool::new(false));
                media.connect_unprepared({
                    let sd = shutdown.clone();
                    move |_| sd.store(true, Ordering::Relaxed)
                });
                if let Some(a) = bin.by_name("audiosrc") {
                    let audiosrc = a.downcast::<gst_app::AppSrc>().unwrap();
                    let sd = shutdown.clone();
                    std::thread::spawn(move || run_audio_output(audiosrc, audio_port, sd));
                }
                std::thread::spawn(move || run_capture_output(appsrc, shutdown));
            });
        }

        Mode::Window => {
            // Explicit CLI dimensions take priority; fall back to querying wl_output
            let dynamic_size = args.width.is_none() || args.height.is_none();
            let (default_w, default_h) = args.width.zip(args.height)
                .or_else(|| query_output_size())
                .unwrap_or_else(|| {
                    eprintln!("toplevel: could not query wl_output size; falling back to 1920×1080");
                    (1920, 1080)
                });

            // Placeholder launch string; the real pipeline is injected via take_pipeline
            factory.set_launch(&format!(
                "videotestsrc ! video/x-raw,format=I420,width=16,height=16,framerate={fps}/1 \
                 ! x264enc speed-preset=ultrafast ! rtph264pay name=pay0 pt=96",
                fps = framerate()
            ));
            factory.connect_media_configure(move |_, media| {
                let shutdown = Arc::new(AtomicBool::new(false));
                media.connect_unprepared({
                    let sd = shutdown.clone();
                    move |_| sd.store(true, Ordering::Relaxed)
                });
                run_capture_toplevel(media, default_w, default_h, dynamic_size, shutdown);
            });
        }
    }

    if args.http_port != 0 {
        let addr = format!("{}:{}", args.bind_address, args.http_port);
        match TcpListener::bind(&addr) {
            Ok(listener) => {
                println!("WebM stream:  http://{addr}/");
                println!("MKV stream:   http://{addr}/stream.mkv");
                println!("MJPEG stream: http://{addr}/mjpeg");
                println!("Snapshot:     http://{addr}/snapshot");
                println!("Metrics:      http://{addr}/metrics");
                std::thread::spawn(move || serve_http(listener));
            }
            Err(e) => eprintln!("http: cannot bind {addr}: {e}"),
        }
    }

    // ssdp makes dlna players list the stream; a byebye goes out on shutdown
    // so they drop it instead of timing out on the announcement
    let ssdp_shutdown = Arc::new(AtomicBool::new(false));
    if args.http_port != 0 && !args.no_dlna {
        let _ = DLNA_NAME.set(args.dlna_name.clone());
        dlna::spawn_ssdp(args.http_port, ssdp_shutdown.clone());
    }

    let mounts = server.mount_points().expect("mount points");
    mounts.add_factory("/stream", factory.clone());
    server.attach(None).expect("RTSP server attach");
    println!("RTSP server: rtsp://{}:{}/stream", args.bind_address, args.bind_port);

    let main_loop = glib::MainLoop::new(None, false);
    let mut signals = Signals::new([SIGINT, SIGTERM]).expect("signals");
    let ml = main_loop.clone();
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            ssdp_shutdown.store(true, Ordering::Relaxed);
            // One poll interval, so the ssdp thread can send its byebye
            std::thread::sleep(Duration::from_millis(1200));
            ml.quit();
        }
    });
    main_loop.run();
}
