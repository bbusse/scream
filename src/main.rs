// scream - Screen Stream
//
// SPDX-License-Identifier: BSD-3-Clause
// Copyright (c) 2026 Björn Busse
//

use clap::Parser;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use gstreamer_rtsp_server::prelude::*;
use gstreamer_rtsp_server::{RTSPMediaFactory, RTSPServer};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::os::unix::io::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wayland_client::protocol::{
	wl_buffer::WlBuffer,
	wl_output::WlOutput,
	wl_registry::{self, WlRegistry},
	wl_shm::{self, WlShm},
	wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
	ext_image_capture_source_v1::ExtImageCaptureSourceV1,
	ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
	ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
	ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
	ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
	/// Address to bind the RTSP server to
	#[arg(long, default_value = "0.0.0.0")]
	bind_address: String,
	/// Port to bind the RTSP server to
	#[arg(long, default_value = "7001")]
	bind_port: String,
}

/// Wayland connection state: discovered globals plus the live session/frame
/// constraints reported by the compositor.
#[derive(Default)]
struct State {
	output: Option<WlOutput>,
	shm: Option<WlShm>,
	source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
	capture_manager: Option<ExtImageCopyCaptureManagerV1>,

	width: u32,
	height: u32,
	shm_format: Option<wl_shm::Format>,
	/// Set on every `done` event; cleared once buffer_size/shm_format have
	/// been consumed. The compositor may resend constraints mid-session
	/// (e.g. on a resolution change), so this isn't a one-shot flag.
	constraints_dirty: bool,
	session_stopped: bool,

	frame_result: Option<FrameResult>,
}

enum FrameResult {
	Ready,
	Failed(WEnum<ext_image_copy_capture_frame_v1::FailureReason>),
}

impl Dispatch<WlRegistry, ()> for State {
	fn event(
		state: &mut Self,
		registry: &WlRegistry,
		event: <WlRegistry as Proxy>::Event,
		_data: &(),
		_conn: &Connection,
		qh: &QueueHandle<Self>,
	) {
		if let wl_registry::Event::Global { name, interface, version } = event {
			match interface.as_str() {
				"wl_output" => {
					if state.output.is_none() {
						state.output = Some(registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ()));
					} else {
						eprintln!("Ignoring additional wl_output global (name={name}); using the first one found");
					}
				}
				"wl_shm" => state.shm = Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ())),
				"ext_output_image_capture_source_manager_v1" => {
					state.source_manager =
						Some(registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(name, version.min(1), qh, ()));
				}
				"ext_image_copy_capture_manager_v1" => {
					state.capture_manager =
						Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(name, version.min(1), qh, ()));
				}
				_ => {}
			}
		}
	}
}

impl Dispatch<WlOutput, ()> for State {
	fn event(_: &mut Self, _: &WlOutput, _: <WlOutput as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShm, ()> for State {
	fn event(_: &mut Self, _: &WlShm, _: <WlShm as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShmPool, ()> for State {
	fn event(_: &mut Self, _: &WlShmPool, _: <WlShmPool as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlBuffer, ()> for State {
	fn event(_: &mut Self, _: &WlBuffer, _: <WlBuffer as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for State {
	fn event(
		_: &mut Self,
		_: &ExtOutputImageCaptureSourceManagerV1,
		_: <ExtOutputImageCaptureSourceManagerV1 as Proxy>::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}
impl Dispatch<ExtImageCaptureSourceV1, ()> for State {
	fn event(
		_: &mut Self,
		_: &ExtImageCaptureSourceV1,
		_: <ExtImageCaptureSourceV1 as Proxy>::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}
impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State {
	fn event(
		_: &mut Self,
		_: &ExtImageCopyCaptureManagerV1,
		_: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
		_: &(),
		_: &Connection,
		_: &QueueHandle<Self>,
	) {
	}
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
	fn event(
		state: &mut Self,
		_proxy: &ExtImageCopyCaptureSessionV1,
		event: <ExtImageCopyCaptureSessionV1 as Proxy>::Event,
		_data: &(),
		_conn: &Connection,
		_qh: &QueueHandle<Self>,
	) {
		use ext_image_copy_capture_session_v1::Event;
		match event {
			Event::BufferSize { width, height } => {
				state.width = width;
				state.height = height;
			}
			// Prefer the first format we understand; later events in the same
			// batch shouldn't override an already-chosen one.
			Event::ShmFormat { format: WEnum::Value(format) }
				if state.shm_format.is_none() && matches!(format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888) =>
			{
				state.shm_format = Some(format);
			}
			Event::Done => state.constraints_dirty = true,
			Event::Stopped => state.session_stopped = true,
			_ => {}
		}
	}
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
	fn event(
		state: &mut Self,
		_proxy: &ExtImageCopyCaptureFrameV1,
		event: <ExtImageCopyCaptureFrameV1 as Proxy>::Event,
		_data: &(),
		_conn: &Connection,
		_qh: &QueueHandle<Self>,
	) {
		match event {
			ext_image_copy_capture_frame_v1::Event::Ready => state.frame_result = Some(FrameResult::Ready),
			ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
				state.frame_result = Some(FrameResult::Failed(reason));
			}
			// transform, damage, presentation_time: not needed for a raw RGB dump
			_ => {}
		}
	}
}

/// A wl_shm-backed buffer: shared memory allocated via an unnamed temp file,
/// wrapped in wl_shm_pool/wl_buffer so the compositor can copy frames into it.
struct ShmBuffer {
	_file: std::fs::File,
	mmap: memmap2::MmapMut,
	pool: WlShmPool,
	buffer: WlBuffer,
	width: u32,
	height: u32,
	stride: i32,
}

impl ShmBuffer {
	fn new(shm: &WlShm, qh: &QueueHandle<State>, width: u32, height: u32, format: wl_shm::Format) -> Self {
		let stride = width as i32 * 4;
		let size = stride as i64 * height as i64;

		let file = tempfile::tempfile().expect("Failed to create shm-backed temp file");
		file.set_len(size as u64).expect("Failed to size shm buffer");
		let mmap = unsafe { memmap2::MmapMut::map_mut(&file).expect("Failed to mmap shm buffer") };

		let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
		let buffer = pool.create_buffer(0, width as i32, height as i32, stride, format, qh, ());

		ShmBuffer { _file: file, mmap, pool, buffer, width, height, stride }
	}
}

impl Drop for ShmBuffer {
	fn drop(&mut self) {
		self.buffer.destroy();
		self.pool.destroy();
	}
}

fn gst_video_format(format: wl_shm::Format) -> Option<&'static str> {
	// Wayland shm formats name the pixel value as 0xAARRGGBB / 0x00RRGGBB;
	// on a little-endian host that's byte order B,G,R,A in memory, which is
	// GStreamer's BGRA/BGRx.
	match format {
		wl_shm::Format::Argb8888 => Some("BGRA"),
		wl_shm::Format::Xrgb8888 => Some("BGRx"),
		_ => None,
	}
}

/// Connects to the compositor, negotiates a screen-capture session over
/// ext-image-copy-capture-v1 and continuously pushes captured frames into
/// `appsrc` until told to shut down or the compositor stops the session.
///
/// The Wayland side and the appsrc feed are deliberately decoupled: the
/// protocol allows the compositor to wait indefinitely for screen content
/// to change before completing a capture, so a static screen must not stall
/// the RTSP session (its preroll needs a steady trickle of buffers). A
/// dedicated capture thread updates `latest_frame` whenever the compositor
/// produces one; this thread ticks on a fixed cadence and re-sends whatever
/// is newest, duplicating the last frame if nothing changed.
fn run_capture(appsrc: gst_app::AppSrc, shutdown: Arc<AtomicBool>) {
	let latest_frame: Arc<Mutex<Option<LatestFrame>>> = Arc::new(Mutex::new(None));

	{
		let latest_frame = latest_frame.clone();
		let shutdown = shutdown.clone();
		std::thread::spawn(move || wayland_capture_loop(latest_frame, shutdown));
	}

	let mut caps_set = false;
	while !shutdown.load(Ordering::Relaxed) {
		if let Some(frame) = latest_frame.lock().unwrap().clone() {
			if !caps_set {
				let caps = gst::Caps::builder("video/x-raw")
					.field("format", frame.gst_format)
					.field("width", frame.width as i32)
					.field("height", frame.height as i32)
					.field("framerate", gst::Fraction::new(30, 1))
					.build();
				appsrc.set_caps(Some(&caps));
				caps_set = true;
			}
			let gst_buffer = gst::Buffer::from_slice(frame.pixels);
			if appsrc.push_buffer(gst_buffer).is_err() {
				return; // Downstream pipeline is gone (no clients / media torn down).
			}
		}
		// Matches the declared caps framerate above; a real capture faster
		// than this would otherwise be silently throttled down to whatever
		// this tick interval allows.
		std::thread::sleep(Duration::from_millis(33));
	}
}

#[derive(Clone)]
struct LatestFrame {
	pixels: Vec<u8>,
	width: u32,
	height: u32,
	gst_format: &'static str,
}

/// Connects to the compositor, negotiates an ext-image-copy-capture-v1
/// session and keeps `latest_frame` updated with the newest capture.
fn wayland_capture_loop(latest_frame: Arc<Mutex<Option<LatestFrame>>>, shutdown: Arc<AtomicBool>) {
	let conn = match Connection::connect_to_env() {
		Ok(conn) => conn,
		Err(err) => {
			eprintln!("Failed to connect to Wayland: {err}");
			return;
		}
	};
	let mut event_queue: EventQueue<State> = conn.new_event_queue();
	let qh = event_queue.handle();
	let _registry = conn.display().get_registry(&qh, ());

	let mut state = State::default();
	if let Err(err) = event_queue.roundtrip(&mut state) {
		eprintln!("Initial Wayland roundtrip failed: {err}");
		return;
	}

	let (Some(output), Some(shm), Some(source_manager), Some(capture_manager)) =
		(state.output.take(), state.shm.take(), state.source_manager.take(), state.capture_manager.take())
	else {
		eprintln!(
			"Compositor is missing wl_output, wl_shm, ext_output_image_capture_source_manager_v1 \
			 or ext_image_copy_capture_manager_v1 - screen capture is unavailable"
		);
		return;
	};

	let source = source_manager.create_source(&output, &qh, ());
	let session = capture_manager.create_session(&source, Options::empty(), &qh, ());

	// Wait for the first full batch of buffer constraints.
	while !state.constraints_dirty && !state.session_stopped && !shutdown.load(Ordering::Relaxed) {
		if let Err(err) = event_queue.blocking_dispatch(&mut state) {
			eprintln!("Wayland dispatch failed: {err}");
			return;
		}
	}
	if shutdown.load(Ordering::Relaxed) {
		return;
	}

	let mut shm_buffer: Option<ShmBuffer> = None;
	let mut gst_format: &'static str = "";

	while !shutdown.load(Ordering::Relaxed) {
		if state.session_stopped {
			eprintln!("Capture session stopped by compositor");
			return;
		}

		if state.constraints_dirty {
			state.constraints_dirty = false;
			// Reset so a resent batch (e.g. on a resolution change) is
			// evaluated on its own terms instead of keeping a stale format
			// the compositor may no longer be offering.
			let Some(format) = state.shm_format.take() else {
				eprintln!("Compositor advertised no supported shm format (need Xrgb8888 or Argb8888)");
				return;
			};

			const MAX_DIMENSION: u32 = 16384; // generous bound; no real output gets close
			if state.width == 0 || state.height == 0 || state.width > MAX_DIMENSION || state.height > MAX_DIMENSION {
				eprintln!("Compositor reported implausible buffer size {}x{}", state.width, state.height);
				return;
			}

			let needs_realloc =
				shm_buffer.as_ref().is_none_or(|b| b.width != state.width || b.height != state.height);
			if needs_realloc {
				shm_buffer = Some(ShmBuffer::new(&shm, &qh, state.width, state.height, format));
			}
			let Some(format) = gst_video_format(format) else {
				eprintln!("Unsupported shm format: {format:?}");
				return;
			};
			gst_format = format;
		}

		let Some(buf) = shm_buffer.as_ref() else {
			// No usable constraints yet; wait for another batch.
			if let Err(err) = event_queue.blocking_dispatch(&mut state) {
				eprintln!("Wayland dispatch failed: {err}");
				return;
			}
			continue;
		};

		let frame = session.create_frame(&qh, ());
		frame.attach_buffer(&buf.buffer);
		frame.damage_buffer(0, 0, buf.width as i32, buf.height as i32);
		frame.capture();

		state.frame_result = None;
		while state.frame_result.is_none() && !state.session_stopped && !shutdown.load(Ordering::Relaxed) {
			if let Err(err) = event_queue.blocking_dispatch(&mut state) {
				eprintln!("Wayland dispatch failed: {err}");
				frame.destroy();
				return;
			}
		}
		frame.destroy();

		match state.frame_result.take() {
			Some(FrameResult::Ready) => {
				let pixels = buf.mmap[..(buf.stride as usize * buf.height as usize)].to_vec();
				*latest_frame.lock().unwrap() =
					Some(LatestFrame { pixels, width: buf.width, height: buf.height, gst_format });
			}
			Some(FrameResult::Failed(reason)) => {
				eprintln!("Frame capture failed: {reason:?}");
				std::thread::sleep(Duration::from_millis(100));
			}
			None => return, // shutdown requested or session stopped mid-wait
		}
	}
}

fn main() {
	// Set GST_DEBUG from env, default to 2
	if std::env::var("GST_DEBUG").is_err() {
		std::env::set_var("GST_DEBUG", "2");
	}

	// Initialize GStreamer
	gst::init().expect("Failed to initialize GStreamer");

	// Parse CLI arguments
	let args = Args::parse();

	// Create RTSP server
	let server = RTSPServer::new();
	server.set_address(&args.bind_address);
	server.set_service(&args.bind_port);

	// Create media factory with appsrc pipeline. The placeholder caps are
	// replaced with the real capture size/format as soon as it's known.
	let factory = RTSPMediaFactory::new();
	factory.set_launch(
		"appsrc name=mysrc is-live=true do-timestamp=true format=time caps=video/x-raw,format=BGRx,width=16,height=16,framerate=30/1 ! videoconvert ! video/x-raw,format=I420 ! x264enc speed-preset=ultrafast tune=zerolatency ! rtph264pay name=pay0 pt=96"
	);
	factory.set_shared(true);

	// Attach factory to RTSP server
	let mounts = server.mount_points().expect("Failed to get mount points");
	mounts.add_factory("/stream", factory.clone());

	// Start server
	server.attach(None).expect("Failed to attach RTSP server to the main context");
	println!("RTSP server is running at rtsp://{}:{}/stream", args.bind_address, args.bind_port);

	// GStreamer: Use factory's "media-configure" signal to access appsrc and
	// kick off a capture thread for it. Without an active session keeping it
	// alive, gst-rtsp-server tears the shared media down between requests
	// and media-configure fires again on the next one - each fresh instance
	// gets its own capture thread, tied to that media's own teardown
	// (`unprepared`) so repeated connects don't leak threads.
	factory.connect_media_configure(move |_, media| {
		let element = media.element();
		let bin = element.downcast_ref::<gst::Bin>().expect("Failed to downcast element to Bin");
		let appsrc = bin.by_name("mysrc").expect("appsrc not found").downcast::<gst_app::AppSrc>().expect("Failed to downcast to AppSrc");

		let media_shutdown = Arc::new(AtomicBool::new(false));
		media.connect_unprepared({
			let media_shutdown = media_shutdown.clone();
			move |_| media_shutdown.store(true, Ordering::Relaxed)
		});

		std::thread::spawn(move || run_capture(appsrc, media_shutdown));
	});

	let main_loop = glib::MainLoop::new(None, false);

	// Graceful shutdown on SIGINT/SIGTERM
	let mut signals = Signals::new([SIGINT, SIGTERM]).expect("Failed to install signal handler");
	let main_loop_for_signal = main_loop.clone();
	std::thread::spawn(move || {
		if signals.forever().next().is_some() {
			main_loop_for_signal.quit();
		}
	});

	main_loop.run();
}
