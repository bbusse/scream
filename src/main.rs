// scream: Screen Stream
//
// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2026 Björn Busse

use clap::Parser;
use gstreamer::{self as gst, prelude::*};

use scream::dlna;
use scream::hls;
use scream::http::Request;
use scream::layout::Grid;
use scream::metrics::{
    self, ClientGuard, CLIENTS_HLS, CLIENTS_MJPEG, CLIENTS_MKV, CLIENTS_RTSP,
    CLIENTS_SNAPSHOT, CLIENTS_TS, CLIENTS_WEBM, SNAPSHOTS_TOTAL,
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
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};
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
    /// Composite canvas width, defaults to wl_output width (toplevel mode only)
    #[arg(long)]
    width: Option<u32>,
    /// Composite canvas height, defaults to wl_output height (toplevel mode only)
    #[arg(long)]
    height: Option<u32>,
    /// Frames per second to capture and encode
    #[arg(long, default_value = "30")]
    framerate: u32,
    /// Port for the http mjpeg stream a browser can display, 0 disables it
    #[arg(long, default_value = "7002")]
    http_port: u16,
    /// Port the media player relays stream audio to, opus over rtp, 0 disables
    #[arg(long, env = "STREAM_AUDIO_PORT", default_value = "7005")]
    audio_port: u16,
    /// Do not announce the stream over ssdp for dlna players
    #[arg(long)]
    no_dlna: bool,
    /// What clients call the stream: the RTSP SDP session name, the WebM and
    /// Matroska container title, and the name dlna players list it under
    #[arg(long, env = "STREAM_TITLE", default_value = "ISS Display")]
    stream_title: String,
    /// Base URL clients should reach this stream at, e.g.
    /// http://192.168.1.10:7002. Overrides the address scream autodetects for
    /// the ssdp LOCATION and the DLNA stream url. Needed behind a NAT or in a
    /// container where the published port differs from the internal one
    #[arg(long, env = "STREAM_ADVERTISE_URL")]
    advertise_url: Option<String>,
    /// Tunnel ssdp over one unicast udp connection to a `scream
    /// --ssdp-relay-server` at this host:port instead of speaking multicast
    /// directly. Use when multicast cannot leave the container/VM, e.g.
    /// host.containers.internal:1901
    #[arg(long, env = "STREAM_SSDP_RELAY")]
    ssdp_relay: Option<String>,
    /// Run as the ssdp relay: bridge the LAN multicast group to scream
    /// instances that connect over --ssdp-relay. Ignores all capture options
    /// and runs until killed
    #[arg(long)]
    ssdp_relay_server: bool,
    /// Address the relay accepts scream instances on (--ssdp-relay-server)
    #[arg(long, default_value = "0.0.0.0:1901")]
    ssdp_relay_listen: String,
    /// Local IPv4 address to join the ssdp group on (--ssdp-relay-server),
    /// 0.0.0.0 uses the default-route interface
    #[arg(long, default_value = "0.0.0.0")]
    ssdp_relay_iface: String,
    /// UDP port the relay listens for the ssdp group on (--ssdp-relay-server),
    /// 1900 is the standard and what clients send M-SEARCH to
    #[arg(long, default_value = "1900")]
    ssdp_relay_lan_port: u16,
}

static STREAM_TITLE: OnceLock<String> = OnceLock::new();

fn stream_title() -> &'static str {
    STREAM_TITLE.get().map(|s| s.as_str()).unwrap_or("ISS Display")
}

static FRAMERATE: AtomicU32 = AtomicU32::new(30);

fn framerate() -> u32 {
    FRAMERATE.load(Ordering::Relaxed)
}

// gst-rtsp-server hardcodes the SDP session name as "Session streamed with
// GStreamer" in its client class. A RTSPMedia subclass whose setup_sdp runs
// after the default one rewrites the s= and i= lines with our own title. The
// factory is told to make these instead of plain RTSPMedia via set_media_gtype
mod titled_media {
    use gstreamer_rtsp_server::subclass::prelude::*;

    mod imp {
        use super::*;
        use gstreamer as gst;

        #[derive(Default)]
        pub struct TitledMedia;

        #[glib::object_subclass]
        impl ObjectSubclass for TitledMedia {
            const NAME: &'static str = "ScreamTitledMedia";
            type Type = super::TitledMedia;
            type ParentType = gstreamer_rtsp_server::RTSPMedia;
        }

        impl ObjectImpl for TitledMedia {}

        impl RTSPMediaImpl for TitledMedia {
            fn setup_sdp(
                &self,
                sdp: &mut gstreamer_sdp::SDPMessageRef,
                info: &gstreamer_rtsp_server::subclass::SDPInfo,
            ) -> Result<(), gst::LoggableError> {
                self.parent_setup_sdp(sdp, info)?;
                let title = crate::stream_title();
                sdp.set_session_name(title);
                sdp.set_information(title);
                Ok(())
            }
        }
    }

    glib::wrapper! {
        pub struct TitledMedia(ObjectSubclass<imp::TitledMedia>)
            @extends gstreamer_rtsp_server::RTSPMedia;
    }
}

fn frame_interval() -> Duration {
    Duration::from_secs(1) / framerate().max(1)
}

// Paces a feed loop at the frame rate without the drift a sleep after the
// work adds. A turn that overran starts the next one right away
fn next_turn(deadline: &mut Instant) {
    *deadline += frame_interval();
    let now = Instant::now();
    match deadline.checked_duration_since(now) {
        Some(wait) => std::thread::sleep(wait),
        None => *deadline = now,
    }
}

// Where the pipeline clock stands for this element, None until it is PLAYING
fn running_time(el: &gst_app::AppSrc) -> Option<gst::ClockTime> {
    Some(el.clock()?.time().saturating_sub(el.base_time()?))
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
    // Shared, so a consumer's clone and the buffer wrapping it are refcounts,
    // not more copies of the frame
    pixels: Arc<[u8]>,
    width: u32,
    height: u32,
    gst_format: &'static str,
}

type FrameSlot = Arc<Mutex<Option<LatestFrame>>>;

// Capturing an output costs a few percent of a core, encoding it is what costs
// real time. So one capture feeds every consumer and the encoders start only
// when something asks for them: h264 when an rtsp client connects, jpeg while
// an http client is reading
static FRAMES: OnceLock<FrameSlot> = OnceLock::new();
static CAPTURE_RUNNING: AtomicBool = AtomicBool::new(false);
static CAPTURE_SHUTDOWN: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn frames() -> &'static FrameSlot {
    FRAMES.get_or_init(|| Arc::new(Mutex::new(None)))
}

fn latest_frame() -> Option<LatestFrame> {
    frames().lock().ok()?.clone()
}

fn video_caps(frame: &LatestFrame) -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("format", frame.gst_format)
        .field("width", frame.width as i32)
        .field("height", frame.height as i32)
        .field("framerate", gst::Fraction::new(framerate() as i32, 1))
        .build()
}

// One raw video appsrc. Caps come from the first frame. Frames wait until the
// pipeline is PLAYING and has a clock, since do-timestamp stamps with it at
// push and an appsrc without one emits them unstamped
struct VideoFeed {
    src: gst_app::AppSrc,
    caps_set: bool,
}

impl VideoFeed {
    fn new(src: gst_app::AppSrc) -> Self {
        VideoFeed { src, caps_set: false }
    }

    fn push(&mut self, frame: LatestFrame) -> bool {
        if self.src.clock().is_none() {
            return true;
        }
        if !self.caps_set {
            self.src.set_caps(Some(&video_caps(&frame)));
            self.caps_set = true;
        }
        self.src.push_buffer(gst::Buffer::from_slice(frame.pixels)).is_ok()
    }
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
// capture thread decodes it to raw pcm and fans each chunk out to every
// subscriber, so the rtsp pay1 and each webm client hear the whole stream
// instead of splitting chunks between however many are listening at once
//
// Each chunk carries the clock time it left the jitterbuffer. Every pipeline
// here runs on the one system clock, so an output pipeline places a chunk on
// its own running time by subtracting its base time, and the audio lands
// where video stamped by that same clock lands. Metering chunks out at the
// video feed's pace, as this used to, stamped two chunks alike whenever two
// fell into one turn and dropped one in fourteen because the pace ran a
// little slow, which a sleep after the work always does
type PcmChunk = (gst::ClockTime, Vec<u8>);
type PcmQueue = Arc<Mutex<VecDeque<PcmChunk>>>;
type PcmSubscribers = Mutex<Vec<Weak<Mutex<VecDeque<PcmChunk>>>>>;
static AUDIO_SUBSCRIBERS: OnceLock<PcmSubscribers> = OnceLock::new();
static AUDIO_CAPTURE: AtomicBool = AtomicBool::new(false);
// Set once from Args so the http handlers can reach it, 0 disables audio
static AUDIO_PORT: AtomicU16 = AtomicU16::new(0);

fn audio_port() -> u16 {
    AUDIO_PORT.load(Ordering::Relaxed)
}

// 20 ms of 48 kHz interleaved stereo s16, the opusdec output chunk
const AUDIO_CHUNK: gst::ClockTime = gst::ClockTime::from_mseconds(20);
const AUDIO_CHUNK_BYTES: usize = 960 * 2 * 2;
// The media player sends over loopback, so this covers ordinary scheduling
// jitter without the far larger fixed delay a real network hop would need
const AUDIO_JITTER: gst::ClockTime = gst::ClockTime::from_mseconds(60);
// How far behind the clock the audio timeline may fall before silence pads
// it. Longer than a chunk waits for the next feed turn, so real audio is
// never displaced by padding
const AUDIO_QUIET_AFTER: gst::ClockTime = gst::ClockTime::from_mseconds(100);
// A consumer drains its queue every feed turn, so this only bounds what a
// stalled one piles up
const AUDIO_QUEUE_CHUNKS: usize = 50;

fn audio_subscribers() -> &'static PcmSubscribers {
    AUDIO_SUBSCRIBERS.get_or_init(|| Mutex::new(Vec::new()))
}

// A new queue of this consumer's own, registered to receive every chunk the
// decode thread produces from here on
fn subscribe_audio() -> PcmQueue {
    let q: PcmQueue = Arc::new(Mutex::new(VecDeque::new()));
    audio_subscribers().lock().unwrap().push(Arc::downgrade(&q));
    q
}

fn audio_caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .field("rate", 48_000i32)
        .field("channels", 2i32)
        .field("layout", "interleaved")
        .build()
}

fn ensure_audio_capture(port: u16) {
    if port == 0 || AUDIO_CAPTURE.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let desc = format!(
            "udpsrc port={port} \
             caps=application/x-rtp,media=audio,encoding-name=OPUS,clock-rate=48000,payload=97 \
             ! rtpjitterbuffer latency={jitter} mode=none do-lost=true \
             ! rtpopusdepay ! opusdec plc=true \
             ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
             ! appsink name=asink sync=false max-buffers=64 drop=true",
            jitter = AUDIO_JITTER.mseconds()
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
                    let Some(buffer) = sample.buffer() else { continue };
                    let (Some(pts), Some(base), Ok(map)) =
                        (buffer.pts(), pipeline.base_time(), buffer.map_readable())
                    else {
                        continue;
                    };
                    let chunk = (pts + base + AUDIO_JITTER, map.as_slice().to_vec());
                    let mut subs = audio_subscribers().lock().unwrap();
                    subs.retain(|weak| {
                        let Some(q) = weak.upgrade() else { return false };
                        if let Ok(mut q) = q.lock() {
                            q.push_back(chunk.clone());
                            while q.len() > AUDIO_QUEUE_CHUNKS {
                                q.pop_front();
                            }
                        }
                        true
                    });
                }
                None if asink.is_eos() => break,
                None => {}
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        AUDIO_CAPTURE.store(false, Ordering::SeqCst);
    });
}

// The audio timeline of one output pipeline. Its queue is drained every feed
// turn, each chunk stamped where its due time falls on this pipeline's
// running time, so the feed loop's pace can neither stretch nor squeeze the
// audio. Silence keeps the timeline going while nothing arrives, so a
// client's audio branch prerolls and plays when no media view is up
struct AudioFeed {
    src: gst_app::AppSrc,
    queue: PcmQueue,
    next_pts: Option<gst::ClockTime>,
}

impl AudioFeed {
    fn new(src: gst_app::AppSrc, port: u16) -> Self {
        ensure_audio_capture(port);
        AudioFeed { src, queue: subscribe_audio(), next_pts: None }
    }

    fn push(&mut self) -> bool {
        let (Some(now), Some(base)) = (running_time(&self.src), self.src.base_time()) else {
            return true;
        };
        let mut next = *self.next_pts.get_or_insert(now);
        loop {
            let chunk = self.queue.lock().ok().and_then(|mut q| q.pop_front());
            let (pts, pcm) = match chunk {
                Some((due, pcm)) => (due.saturating_sub(base), pcm),
                None if next + AUDIO_QUIET_AFTER <= now => (next, vec![0u8; AUDIO_CHUNK_BYTES]),
                None => break,
            };
            // Already covered by silence, or a stale burst after the source
            // stalled. Dropping it keeps the timeline on the clock instead of
            // letting it run ahead of the video
            if pts + AUDIO_CHUNK / 2 < next {
                continue;
            }
            let mut buf = gst::Buffer::from_slice(pcm);
            {
                let buf = buf.get_mut().unwrap();
                buf.set_pts(pts);
                buf.set_duration(AUDIO_CHUNK);
            }
            if self.src.push_buffer(buf).is_err() {
                return false;
            }
            next = pts + AUDIO_CHUNK;
        }
        self.next_pts = Some(next);
        true
    }
}

// A persistent pipeline: building one per frame would cost more than the encode
struct JpegEncoder {
    video: VideoFeed,
    sink: gst_app::AppSink,
    pipeline: gst::Pipeline,
}

impl JpegEncoder {
    fn new() -> Result<Self, String> {
        ensure_capture();
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

        Ok(JpegEncoder { video: VideoFeed::new(appsrc), sink, pipeline })
    }

    fn encode(&mut self, frame: LatestFrame) -> Option<Vec<u8>> {
        self.video.push(frame).then_some(())?;
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

// A finite response (/metrics, /snapshot) leaves the connection open for the
// next request, so a scrape from Prometheus or the controller every few
// seconds reuses one socket instead of leaving a TIME_WAIT behind each time.
// Streaming responses and errors still take the connection down
enum HttpNext {
    KeepAlive,
    Close,
}

fn serve_http(listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        std::thread::spawn(move || {
            let mut stream = stream;
            // Stops an idle kept-alive connection from pinning its thread
            let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
            loop {
                match handle_http(&mut stream) {
                    Ok(HttpNext::KeepAlive) => continue,
                    Ok(HttpNext::Close) => break,
                    Err(e) => {
                        log_http(&format!("client gone: {e}"));
                        break;
                    }
                }
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

fn handle_http(stream: &mut TcpStream) -> std::io::Result<HttpNext> {
    // Bound in a let so the BufReader temporary is dropped before the arms
    // below borrow the stream to write
    let parsed = Request::parse(&mut std::io::BufReader::new(&*stream));
    let request = match parsed {
        Some(r) => r,
        // A clean close between requests, or the read timeout firing, both
        // land here, there is nothing to reply to
        None => return Ok(HttpNext::Close),
    };

    let path = request.path().to_string();

    // The dlna endpoints answer descriptions and SOAP, not video: they must
    // not spin up capture or an encoder
    if dlna::handle_request(stream, &request.method, &path,
                            &request.headers, &request.body, stream_title())? {
        return Ok(HttpNext::Close);
    }

    if request.method != "GET" {
        stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Ok(HttpNext::Close);
    }

    // Answered without capture or an encoder, so a scrape never wakes the
    // pipeline. Content-Length and no Connection: close, so a client that
    // scrapes on a timer keeps the one socket
    if path == "/metrics" {
        let body = metrics::metrics_body();
        stream.write_all(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            body.len()).as_bytes())?;
        stream.write_all(body.as_bytes())?;
        return Ok(HttpNext::KeepAlive);
    }

    // hls is many short requests for one encoder, so it has its own lifetime
    // and answers keep-alive like /metrics does
    if path == hls::PLAYLIST_PATH {
        return serve_hls_playlist(stream);
    }
    if let Some(seq) = hls::segment_seq(&path) {
        return serve_hls_segment(stream, seq);
    }

    match path.as_str() {
        "/snapshot" | "/screenshot" => {
            SNAPSHOTS_TOTAL.fetch_add(1, Ordering::Relaxed);
            let _guard = ClientGuard::new(&CLIENTS_SNAPSHOT);
            let Some(mut encoder) = jpeg_encoder(stream)? else {
                return Ok(HttpNext::Close);
            };
            // One frame, for a save button or anything expecting a still
            for _ in 0..40 {
                if let Some(jpeg) = latest_jpeg(&mut encoder) {
                    stream.write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                        jpeg.len()).as_bytes())?;
                    stream.write_all(&jpeg)?;
                    return Ok(HttpNext::KeepAlive);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        }
        "/" | "/webm" => {
            let _guard = ClientGuard::new(&CLIENTS_WEBM);
            match StreamEncoder::webm(audio_port()) {
                Ok(encoder) => stream_chunked(stream, encoder,
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
                Ok(encoder) => stream_chunked(stream, encoder,
                                              "video/x-matroska")?,
                Err(e) => {
                    log_http(&format!("no matroska encoder: {e}"));
                    stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")?;
                }
            }
        }
        "/stream.ts" => {
            let _guard = ClientGuard::new(&CLIENTS_TS);
            match StreamEncoder::mpegts_h264(60, audio_port()) {
                Ok(encoder) => stream_chunked(stream, encoder, "video/mp2t")?,
                Err(e) => {
                    log_http(&format!("no mpeg-ts encoder: {e}"));
                    stream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")?;
                }
            }
        }
        "/mjpeg" => {
            let _guard = ClientGuard::new(&CLIENTS_MJPEG);
            let Some(mut encoder) = jpeg_encoder(stream)? else {
                return Ok(HttpNext::Close);
            };
            stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=screamframe\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
            let mut deadline = Instant::now();
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
                next_turn(&mut deadline);
            }
        }
        _ => {
            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        }
    }

    Ok(HttpNext::Close)
}

// Chunked, so the response never ends and the client keeps playing what
// arrives. Content-Length would be a lie here
fn stream_chunked(stream: &mut TcpStream, mut encoder: StreamEncoder,
                  content_type: &str) -> std::io::Result<()> {
    stream.write_all(format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes())?;

    let mut idle = 0u32;
    let mut deadline = Instant::now();
    loop {
        if let Some(why) = feed_turn(&mut encoder, &mut idle) {
            log_http(why);
            break;
        }
        // Everything muxed this turn as one http chunk: mpegtsmux hands
        // over a buffer per 188 byte packet, one write each would be a
        // syscall storm
        let mut muxed = Vec::new();
        while let Some(chunk) = encoder.pull() {
            muxed.extend_from_slice(&chunk.data);
        }
        if !muxed.is_empty() {
            write!(stream, "{:x}\r\n", muxed.len())?;
            stream.write_all(&muxed)?;
            stream.write_all(b"\r\n")?;
            stream.flush()?;
        }
        next_turn(&mut deadline);
    }
    let _ = stream.write_all(b"0\r\n\r\n");

    Ok(())
}

// One feeder turn: the current frame and the audio since the last turn go
// into the encoder. Some(reason) when the stream cannot go on
fn feed_turn(encoder: &mut StreamEncoder, idle: &mut u32) -> Option<&'static str> {
    match latest_frame() {
        Some(f) => {
            *idle = 0;
            if !encoder.push(f) {
                return Some("appsrc rejected a buffer, closing");
            }
        }
        None => {
            *idle += 1;
            if *idle > 200 {
                return Some("no frames to stream, closing");
            }
        }
    }
    if !encoder.push_audio() {
        return Some("audio appsrc rejected a buffer, closing");
    }
    None
}

// The hls encoder is not tied to a connection: a player fetches the playlist
// and each segment as its own request. So it runs from the first request
// until HLS_IDLE passes without one, and the segments it cut wait in a ring
// for the requests that name them. One encoder however many players
struct HlsState {
    ring: VecDeque<hls::Segment>,
    // Carried across runs so sequence numbers never go backwards for a
    // player that saw the previous run
    next_seq: u64,
    last_request: Instant,
    running: bool,
    // Why the last run could not start, answered as 503 until the next try
    error: Option<String>,
}

struct HlsShared {
    state: Mutex<HlsState>,
    changed: Condvar,
}

static HLS: OnceLock<HlsShared> = OnceLock::new();
const HLS_IDLE: Duration = Duration::from_secs(10);
// Longer than starting the encoder and cutting MIN_SEGMENTS takes
const HLS_START_WAIT: Duration = Duration::from_secs(8);

fn hls_shared() -> &'static HlsShared {
    HLS.get_or_init(|| HlsShared {
        state: Mutex::new(HlsState {
            ring: VecDeque::new(), next_seq: 0, last_request: Instant::now(),
            running: false, error: None,
        }),
        changed: Condvar::new(),
    })
}

// Notes a request and starts the encoder if none runs
fn hls_touch() {
    let mut st = hls_shared().state.lock().unwrap();
    st.last_request = Instant::now();
    if !st.running {
        st.running = true;
        st.error = None;
        std::thread::spawn(hls_run);
    }
}

fn hls_run() {
    let shared = hls_shared();
    let _guard = ClientGuard::new(&CLIENTS_HLS);
    let next_seq = shared.state.lock().unwrap().next_seq;
    let mut segmenter = hls::Segmenter::new(next_seq);
    let started = StreamEncoder::mpegts_h264(framerate() * hls::SEGMENT_SECS, audio_port());
    let error = match started {
        Ok(encoder) => {
            log_http("hls: encoder started");
            hls_feed(encoder, &mut segmenter);
            None
        }
        Err(e) => {
            log_http(&format!("no hls encoder: {e}"));
            Some(e)
        }
    };
    let mut st = shared.state.lock().unwrap();
    st.running = false;
    st.ring.clear();
    st.next_seq = segmenter.next_seq();
    st.error = error;
    shared.changed.notify_all();
}

fn hls_feed(mut encoder: StreamEncoder, segmenter: &mut hls::Segmenter) {
    let shared = hls_shared();
    let mut idle = 0u32;
    let mut deadline = Instant::now();
    loop {
        if let Some(why) = feed_turn(&mut encoder, &mut idle) {
            log_http(&format!("hls: {why}"));
            break;
        }
        while let Some(chunk) = encoder.pull() {
            if let Some(segment) = segmenter.push(&chunk.data, chunk.keyframe, chunk.pts) {
                let mut st = shared.state.lock().unwrap();
                st.ring.push_back(segment);
                while st.ring.len() > hls::RING_SEGMENTS {
                    st.ring.pop_front();
                }
                shared.changed.notify_all();
            }
        }
        if shared.state.lock().unwrap().last_request.elapsed() > HLS_IDLE {
            log_http("hls: no requests, encoder stopped");
            break;
        }
        next_turn(&mut deadline);
    }
}

// Waits for enough segments to start a player, 503 when the encoder cannot
// deliver them. Content-Length and keep-alive: a player refetches the
// playlist every second on the same connection
fn serve_hls_playlist(stream: &mut TcpStream) -> std::io::Result<HttpNext> {
    hls_touch();
    let shared = hls_shared();
    let deadline = Instant::now() + HLS_START_WAIT;
    let mut st = shared.state.lock().unwrap();
    while st.ring.len() < hls::MIN_SEGMENTS && st.error.is_none() {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        st = shared.changed.wait_timeout(st, left).unwrap().0;
    }
    if st.ring.len() < hls::MIN_SEGMENTS {
        drop(st);
        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 2\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n")?;
        return Ok(HttpNext::KeepAlive);
    }
    let body = hls::playlist(&st.ring);
    drop(st);
    stream.write_all(format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()).as_bytes())?;
    stream.write_all(body.as_bytes())?;
    Ok(HttpNext::KeepAlive)
}

fn serve_hls_segment(stream: &mut TcpStream, seq: u64) -> std::io::Result<HttpNext> {
    hls_touch();
    let data = {
        let st = hls_shared().state.lock().unwrap();
        st.ring.iter().find(|s| s.seq == seq).map(|s| s.data.clone())
    };
    match data {
        Some(data) => {
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                data.len()).as_bytes())?;
            stream.write_all(&data)?;
        }
        None => stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n")?,
    }
    Ok(HttpNext::KeepAlive)
}

// One encode-and-mux pipeline per http client. vp8 in webm is what a browser
// <video> plays over a plain chunked http response: no MSE, no signalling and
// no javascript, unlike hls which desktop chrome and firefox will not touch
// without hls.js. h264 in matroska is the same shape for dlna players
struct StreamEncoder {
    video: VideoFeed,
    audio: Option<AudioFeed>,
    sink: gst_app::AppSink,
    pipeline: gst::Pipeline,
}

impl StreamEncoder {
    // vp8 + opus in webm: the browser <video> plays both, and opus is what
    // scream already muxes for rtsp pay1
    fn webm(audio_port: u16) -> Result<Self, String> {
        // deadline=1 is vp8's realtime mode, without it the encoder happily
        // spends far longer than a frame interval on a frame
        let enc = gst::ElementFactory::make("vp8enc")
            .property("deadline", 1i64)
            .property("cpu-used", 8i32)
            .property("threads", 2i32)
            .build().map_err(|e| e.to_string())?;
        let mux = gst::ElementFactory::make("webmmux")
            .property("streamable", true)
            .build().map_err(|e| e.to_string())?;
        let aenc = if audio_port != 0 {
            Some(gst::ElementFactory::make("opusenc")
                .property("bitrate", 96_000i32).build().map_err(|e| e.to_string())?)
        } else {
            None
        };

        Self::build(vec![enc], mux, aenc)
    }

    // The same encode the rtsp stream runs, key_int_max frames between
    // keyframes: a player joining the live stream starts at the next one
    fn x264(key_int_max: u32) -> Result<gst::Element, String> {
        gst::ElementFactory::make("x264enc")
            .property_from_str("speed-preset", "ultrafast")
            .property_from_str("tune", "zerolatency")
            .property("key-int-max", key_int_max)
            .build().map_err(|e| e.to_string())
    }

    // Video only: dlna players are fussy about opus in matroska
    fn matroska_h264() -> Result<Self, String> {
        let mux = gst::ElementFactory::make("matroskamux")
            .property("streamable", true)
            .build().map_err(|e| e.to_string())?;

        Self::build(vec![Self::x264(60)?], mux, None)
    }

    // h264 and aac in mpeg-ts: the dlna live profile, and what an hls player
    // takes. h264parse puts SPS and PPS in front of every keyframe so a
    // segment, or a client joining the chunked stream, decodes from its first
    // frame. Video only when no aac encoder is installed
    fn mpegts_h264(key_int_max: u32, audio_port: u16) -> Result<Self, String> {
        let parse = gst::ElementFactory::make("h264parse")
            .property("config-interval", -1i32)
            .build().map_err(|e| e.to_string())?;
        let mux = gst::ElementFactory::make("mpegtsmux")
            .build().map_err(|e| e.to_string())?;
        let aenc = if audio_port != 0 { aac_encoder() } else { None };

        Self::build(vec![Self::x264(key_int_max)?, parse], mux, aenc)
    }

    fn build(video: Vec<gst::Element>, mux: gst::Element, aenc: Option<gst::Element>)
             -> Result<Self, String> {
        ensure_capture();
        // The container title a browser or dlna player shows for the stream
        if let Some(ts) = mux.dynamic_cast_ref::<gst::TagSetter>() {
            ts.add_tag::<gst::tags::Title>(&stream_title(), gst::TagMergeMode::Replace);
        }

        let pipeline = gst::Pipeline::new();
        // A live appsrc reports no room for latency, the opus encoder needs a
        // frame of it, and the muxer logs the mismatch on every buffer. The
        // appsrc queue is unbounded, so a second of room is honest. appsrc
        // only applies max-latency when min-latency is set as well
        let appsrc = gst_app::AppSrc::builder()
            .is_live(true).do_timestamp(true).format(gst::Format::Time)
            .min_latency(0).max_latency(1_000_000_000).build();
        let convert = gst::ElementFactory::make("videoconvert").build().map_err(|e| e.to_string())?;
        // mpegtsmux emits one buffer per ts packet, a bigger queue than the
        // matroska path needs keeps the feeder from stalling on a keyframe
        let sink = gst_app::AppSink::builder().sync(false).max_buffers(4096).drop(false).build();
        let mut chain = vec![appsrc.upcast_ref::<gst::Element>().clone(), convert];
        chain.extend(video);
        chain.push(mux.clone());
        chain.push(sink.upcast_ref::<gst::Element>().clone());
        pipeline.add_many(&chain).map_err(|e| e.to_string())?;
        gst::Element::link_many(&chain).map_err(|e| e.to_string())?;

        let audio = if let Some(aenc) = aenc {
            let asrc = gst_app::AppSrc::builder()
                .is_live(true).format(gst::Format::Time).caps(&audio_caps()).build();
            let aconv = gst::ElementFactory::make("audioconvert").build().map_err(|e| e.to_string())?;
            let ares = gst::ElementFactory::make("audioresample").build().map_err(|e| e.to_string())?;
            pipeline.add_many([asrc.upcast_ref(), &aconv, &ares, &aenc])
                .map_err(|e| e.to_string())?;
            gst::Element::link_many([asrc.upcast_ref(), &aconv, &ares, &aenc])
                .map_err(|e| e.to_string())?;
            aenc.link(&mux).map_err(|e| e.to_string())?;
            Some(AudioFeed::new(asrc, audio_port()))
        } else {
            None
        };

        pipeline.set_state(gst::State::Playing).map_err(|e| e.to_string())?;

        Ok(StreamEncoder { video: VideoFeed::new(appsrc), audio, sink, pipeline })
    }

    fn push(&mut self, frame: LatestFrame) -> bool {
        self.video.push(frame)
    }

    fn push_audio(&mut self) -> bool {
        self.audio.as_mut().is_none_or(|a| a.push())
    }

    // Only what is already muxed, waiting here would stretch the turn
    fn pull(&mut self) -> Option<Chunk> {
        let sample = self.sink.try_pull_sample(gst::ClockTime::ZERO)?;
        let buffer = sample.buffer()?;
        let map = buffer.map_readable().ok()?;

        Some(Chunk {
            data: map.as_slice().to_vec(),
            keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
            pts: buffer.pts().map(Duration::from).unwrap_or_default(),
        })
    }
}

// One muxed buffer. keyframe and pts are what the hls segmenter cuts on,
// mpegtsmux sets the flag on the packet that starts a video keyframe
struct Chunk {
    data: Vec<u8>,
    keyframe: bool,
    pts: Duration,
}

// The first aac encoder the installed plugins offer. fdkaacenc is in
// gst-plugins-bad next to mpegtsmux, avenc_aac in gst-libav
fn aac_encoder() -> Option<gst::Element> {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let found = ["fdkaacenc", "avenc_aac", "voaacenc"].iter()
        .find_map(|name| gst::ElementFactory::make(name).build().ok());
    if found.is_none() && !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("audio: no aac encoder installed, mpeg-ts and hls are video only");
    }
    found
}

impl Drop for StreamEncoder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn latest_jpeg(encoder: &mut JpegEncoder) -> Option<Vec<u8>> {
    encoder.encode(latest_frame()?)
}

// Coordinator, monitors toplevels + wl_output, fires events

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
        if !poll_events(&conn, &mut eq, &mut state) {
            eprintln!("coordinator: compositor disconnected");
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

// Output size, blocking, one-shot, on the coordinator's state: the wl_output
// modes arrive with the first roundtrip
fn query_output_size() -> Option<(u32, u32)> {
    let conn = Connection::connect_to_env().ok()?;
    let mut eq: EventQueue<CoordState> = conn.new_event_queue();
    conn.display().get_registry(&eq.handle(), ());
    let mut state = CoordState::default();
    eq.roundtrip(&mut state).ok()?;
    eq.roundtrip(&mut state).ok()?;

    (state.output_w > 0 && state.output_h > 0).then_some((state.output_w, state.output_h))
}

// Capture-thread Wayland state
//
// Each capture thread gets its own Wayland connection. An output capture
// binds the output and its source manager, a toplevel capture (target_id
// set) binds the toplevel list to find its handle by identifier and that
// source manager. Session and frame handling are the same from there

#[derive(Default)]
struct CaptureState {
    target_id: String,
    output: Option<WlOutput>,
    shm: Option<WlShm>,
    toplevel_list: Option<ExtForeignToplevelListV1>,
    toplevel_source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    output_source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,
    toplevels: Vec<CapTop>,
    width: u32,
    height: u32,
    shm_format: Option<wl_shm::Format>,
    constraints_dirty: bool,
    session_stopped: bool,
    frame_result: Option<FrameResult>,
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
        let toplevel = !state.target_id.is_empty();
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "ext_image_copy_capture_manager_v1" =>
                    state.capture_manager = Some(registry.bind(name, version.min(1), qh, ())),
                "wl_output" if !toplevel && state.output.is_none() =>
                    state.output = Some(registry.bind(name, version.min(4), qh, ())),
                "ext_output_image_capture_source_manager_v1" if !toplevel =>
                    state.output_source_manager = Some(registry.bind(name, version.min(1), qh, ())),
                "ext_foreign_toplevel_list_v1" if toplevel =>
                    state.toplevel_list = Some(registry.bind(name, version.min(1), qh, ())),
                "ext_foreign_toplevel_image_capture_source_manager_v1" if toplevel =>
                    state.toplevel_source_manager = Some(registry.bind(name, version.min(1), qh, ())),
                _ => {}
            }
        }
    }
}

// Objects whose events carry nothing this side acts on
macro_rules! ignore_events {
    ($($iface:ty),* $(,)?) => {$(
        impl Dispatch<$iface, ()> for CaptureState {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event,
                     _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    WlOutput, WlShm, WlShmPool, WlBuffer,
    ExtForeignToplevelImageCaptureSourceManagerV1,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCaptureSourceV1, ExtImageCopyCaptureManagerV1,
);

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
                // Only the target closing ends this capture, every other
                // window comes and goes through the same list
                ext_foreign_toplevel_handle_v1::Event::Closed if t.identifier == state.target_id => {
                    state.session_stopped = true;
                }
                _ => {}
            }
        }
    }
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

// Reads whatever the compositor has sent and dispatches it, without blocking,
// so a loop that also watches flags can poll. dispatch_pending on its own
// never reads the socket, only a read guard or a blocking dispatch does.
// False once the connection is gone
fn poll_events<S>(conn: &Connection, eq: &mut EventQueue<S>, state: &mut S) -> bool {
    if conn.flush().is_err() {
        return false;
    }
    if let Some(guard) = conn.prepare_read() {
        match guard.read() {
            Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return false,
            Ok(_) => {}
        }
    }
    eq.dispatch_pending(state).is_ok()
}

fn stopped(flags: &[Arc<AtomicBool>]) -> bool {
    flags.iter().any(|f| f.load(Ordering::Relaxed))
}

// Connects, binds the globals for this mode, one roundtrip so they are there
fn connect_capture(target_id: &str, label: &str)
    -> Option<(Connection, EventQueue<CaptureState>, CaptureState)> {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => { eprintln!("{label}: connect: {e}"); return None; }
    };
    let mut eq = conn.new_event_queue();
    conn.display().get_registry(&eq.handle(), ());
    let mut state = CaptureState { target_id: target_id.to_string(), ..Default::default() };
    eq.roundtrip(&mut state).ok()?;

    Some((conn, eq, state))
}

// Waits for the session's constraints, then captures frame after frame into
// the slot until the session stops or a stop flag is raised
fn run_capture_session(
    eq: &mut EventQueue<CaptureState>, state: &mut CaptureState, shm: &WlShm,
    session: &ExtImageCopyCaptureSessionV1, slot: &FrameSlot,
    stop: &[Arc<AtomicBool>], label: &str,
) {
    let qh = eq.handle();
    while !state.constraints_dirty && !state.session_stopped && !stopped(stop) {
        if eq.blocking_dispatch(state).is_err() { return; }
    }

    let mut shm_buf: Option<ShmBuffer> = None;
    let mut gst_format: &'static str = "";

    while !stopped(stop) {
        if state.session_stopped { return; }
        if state.constraints_dirty {
            state.constraints_dirty = false;
            let Some(fmt) = state.shm_format.take() else { return; };
            const MAX: u32 = 16384;
            if state.width == 0 || state.height == 0 || state.width > MAX || state.height > MAX { return; }
            let needs = shm_buf.as_ref().is_none_or(|b| b.width != state.width || b.height != state.height);
            if needs { shm_buf = Some(new_shm_buffer(shm, &qh, state.width, state.height, fmt)); }
            let Some(gf) = gst_video_format(fmt) else { return; };
            gst_format = gf;
        }
        let Some(buf) = shm_buf.as_ref() else {
            if eq.blocking_dispatch(state).is_err() { return; }
            continue;
        };

        let frame = session.create_frame(&qh, ());
        frame.attach_buffer(&buf.buffer);
        frame.damage_buffer(0, 0, buf.width as i32, buf.height as i32);
        frame.capture();

        state.frame_result = None;
        while state.frame_result.is_none() && !state.session_stopped && !stopped(stop) {
            if eq.blocking_dispatch(state).is_err() { frame.destroy(); return; }
        }
        frame.destroy();

        match state.frame_result.take() {
            Some(FrameResult::Ready) => {
                let pixels = Arc::from(&buf.mmap[..(buf.stride as usize * buf.height as usize)]);
                *slot.lock().unwrap() =
                    Some(LatestFrame { pixels, width: buf.width, height: buf.height, gst_format });
            }
            Some(FrameResult::Failed(r)) => {
                eprintln!("{label}: frame failed: {r:?}");
                std::thread::sleep(Duration::from_millis(100));
            }
            None => return,
        }
    }
}

// Finds the target toplevel by its stable identifier, then captures it until
// the window closes, the compositor goes or a stop flag is raised
fn wayland_capture_loop_toplevel(target_id: String, slot: FrameSlot, stop: Vec<Arc<AtomicBool>>) {
    let label = format!("capture[{target_id}]");
    let Some((conn, mut eq, mut state)) = connect_capture(&target_id, &label) else { return };
    let (Some(shm), Some(src_mgr), Some(cap_mgr)) =
        (state.shm.take(), state.toplevel_source_manager.take(), state.capture_manager.take())
    else {
        eprintln!("{label}: missing globals");
        return;
    };

    // The window might arrive after the initial batch if it was just opened
    let deadline = Instant::now() + Duration::from_secs(5);
    let handle = loop {
        if stopped(&stop) || state.session_stopped || !poll_events(&conn, &mut eq, &mut state) { return; }
        if let Some(t) = state.toplevels.iter().find(|t| t.identifier == target_id && t.done) {
            break t.handle.clone();
        }
        if Instant::now() > deadline {
            eprintln!("{label}: toplevel not found");
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let qh = eq.handle();
    let source = src_mgr.create_source(&handle, &qh, ());
    let session = cap_mgr.create_session(&source, Options::empty(), &qh, ());
    run_capture_session(&mut eq, &mut state, &shm, &session, &slot, &stop, &label);
}

// GStreamer compositor pipeline

struct PipelineStream {
    identifier: String,
    slot: FrameSlot,
    video: VideoFeed,
    chain: Vec<gst::Element>, // videoconvert, videoscale, capsfilter, queue
    comp_sink: gst::Pad,
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
    // downstream chain, window streams are added dynamically via add_stream
    fn new(out_w: u32, out_h: u32) -> Self {
        let pipeline = gst::Pipeline::new();

        // SMPTE test card ensures the compositor always has data to output,
        // even before any window streams are attached, also useful for
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
            identifier, slot, video: VideoFeed::new(appsrc),
            chain: vec![convert, scale, cf, queue],
            comp_sink, capture_shutdown,
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
        let _ = stream.video.src.set_state(gst::State::Null);
        self.pipeline.remove(&stream.video.src).ok();
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

// Pipeline manager, receives coordinator events and mutates the live pipeline
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
                eprintln!("toplevel: capturing {app_id}: {title}");
                let slot: FrameSlot = Arc::new(Mutex::new(None));
                let cap_sd = Arc::new(AtomicBool::new(false));
                {
                    let slot = slot.clone();
                    let stop = vec![cap_sd.clone(), shutdown.clone()];
                    let id = identifier.clone();
                    std::thread::spawn(move || wayland_capture_loop_toplevel(id, slot, stop));
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

// Pushes the latest captured frame of each stream once per turn
fn feed_loop(shared: SharedPipeline, shutdown: Arc<AtomicBool>) {
    let mut deadline = Instant::now();
    while !shutdown.load(Ordering::Relaxed) {
        {
            let mut pl = shared.lock().unwrap();
            for s in pl.streams.iter_mut() {
                if let Some(frame) = s.slot.lock().unwrap().clone() {
                    s.video.push(frame);
                }
            }
        }
        next_turn(&mut deadline);
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

fn wayland_capture_loop_output(slot: FrameSlot, shutdown: Arc<AtomicBool>) {
    let label = "output capture";
    let Some((_conn, mut eq, mut state)) = connect_capture("", label) else { return };
    let (Some(output), Some(shm), Some(src_mgr), Some(cap_mgr)) =
        (state.output.take(), state.shm.take(), state.output_source_manager.take(), state.capture_manager.take())
    else {
        eprintln!("{label}: missing globals");
        return;
    };

    let qh = eq.handle();
    let source = src_mgr.create_source(&output, &qh, ());
    let session = cap_mgr.create_session(&source, Options::empty(), &qh, ());
    run_capture_session(&mut eq, &mut state, &shm, &session, &slot, &[shutdown], label);
}

// Feeds mysrc and audiosrc from one loop, video stamped as it is pushed,
// audio where its own due times fall, see AudioFeed
fn run_output(video: gst_app::AppSrc, audio: Option<(gst_app::AppSrc, u16)>,
              shutdown: Arc<AtomicBool>) {
    ensure_capture();
    let mut video = VideoFeed::new(video);
    let mut audio = audio.map(|(src, port)| AudioFeed::new(src, port));
    let mut deadline = Instant::now();
    while !shutdown.load(Ordering::Relaxed) {
        if let Some(frame) = latest_frame() {
            if !video.push(frame) {
                return;
            }
        }
        if !audio.as_mut().is_none_or(|a| a.push()) {
            return;
        }
        next_turn(&mut deadline);
    }
}

fn main() {
    let args = Args::parse();

    // The relay is pure sockets: no compositor, no gstreamer, no rtsp server.
    // Handle it before any of that is touched so it runs anywhere scream builds
    if args.ssdp_relay_server {
        let iface: std::net::Ipv4Addr = args.ssdp_relay_iface.parse()
            .unwrap_or_else(|_| {
                eprintln!("--ssdp-relay-iface must be an IPv4 address");
                std::process::exit(2);
            });
        println!("ssdp-relay: starting");
        if let Err(e) = dlna::run_relay(&args.ssdp_relay_listen, iface, args.ssdp_relay_lan_port) {
            eprintln!("ssdp-relay: {e}");
            std::process::exit(1);
        }
        return;
    }

    if std::env::var("GST_DEBUG").is_err() { std::env::set_var("GST_DEBUG", "2"); }
    gst::init().expect("GStreamer init");

    FRAMERATE.store(args.framerate.max(1), Ordering::Relaxed);
    AUDIO_PORT.store(args.audio_port, Ordering::Relaxed);
    let _ = STREAM_TITLE.set(args.stream_title.clone());
    dlna::set_advertise_url(args.advertise_url.clone());

    let server = RTSPServer::new();
    server.set_address(&args.bind_address);
    server.set_service(&args.bind_port);

    // One connected rtsp client to one open control connection, the count is
    // read back at /metrics
    server.connect_client_connected(|_, client| {
        CLIENTS_RTSP.fetch_add(1, Ordering::Relaxed);
        client.connect_closed(|_| {
            CLIENTS_RTSP.fetch_sub(1, Ordering::Relaxed);
        });
    });

    let factory = RTSPMediaFactory::new();
    factory.set_shared(true);
    factory.set_media_gtype(titled_media::TitledMedia::static_type());

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
                " appsrc name=audiosrc is-live=true format=time \
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
                let audio = bin.by_name("audiosrc")
                    .map(|a| (a.downcast::<gst_app::AppSrc>().unwrap(), audio_port));
                std::thread::spawn(move || run_output(appsrc, audio, shutdown));
            });
        }

        Mode::Window => {
            // Explicit CLI dimensions take priority, fall back to querying wl_output
            let dynamic_size = args.width.is_none() || args.height.is_none();
            let (default_w, default_h) = args.width.zip(args.height)
                .or_else(|| query_output_size())
                .unwrap_or_else(|| {
                    eprintln!("toplevel: could not query wl_output size; falling back to 1920x1080");
                    (1920, 1080)
                });

            // Placeholder launch string, the real pipeline is injected via take_pipeline
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
                println!("TS stream:    http://{addr}/stream.ts");
                println!("HLS stream:   http://{addr}{}", hls::PLAYLIST_PATH);
                println!("MJPEG stream: http://{addr}/mjpeg");
                println!("Snapshot:     http://{addr}/snapshot");
                println!("Metrics:      http://{addr}/metrics");
                std::thread::spawn(move || serve_http(listener));
            }
            Err(e) => eprintln!("http: cannot bind {addr}: {e}"),
        }
    }

    // ssdp makes dlna players list the stream, a byebye goes out on shutdown
    // so they drop it instead of timing out on the announcement
    let ssdp_shutdown = Arc::new(AtomicBool::new(false));
    if args.http_port != 0 && !args.no_dlna {
        dlna::spawn_ssdp(
            dlna::SsdpConfig {
                http_port: args.http_port,
                advertise: args.advertise_url.clone(),
                relay: args.ssdp_relay.clone(),
            },
            ssdp_shutdown.clone(),
        );
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
