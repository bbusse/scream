// Subscriber counts per stream type, exposed at GET /metrics. Each streaming
// handler holds a ClientGuard for as long as its connection is open, the rtsp
// count is kept by the server's client-connected / closed signals

use std::sync::atomic::{AtomicI64, Ordering};

pub static CLIENTS_WEBM: AtomicI64 = AtomicI64::new(0);
pub static CLIENTS_MJPEG: AtomicI64 = AtomicI64::new(0);
pub static CLIENTS_MKV: AtomicI64 = AtomicI64::new(0);
pub static CLIENTS_SNAPSHOT: AtomicI64 = AtomicI64::new(0);
pub static CLIENTS_RTSP: AtomicI64 = AtomicI64::new(0);
pub static SNAPSHOTS_TOTAL: AtomicI64 = AtomicI64::new(0);

pub struct ClientGuard(&'static AtomicI64);

impl ClientGuard {
    pub fn new(counter: &'static AtomicI64) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        ClientGuard(counter)
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

// The Prometheus text exposition for the current counter values
pub fn metrics_body() -> String {
    render(
        CLIENTS_WEBM.load(Ordering::Relaxed),
        CLIENTS_MJPEG.load(Ordering::Relaxed),
        CLIENTS_MKV.load(Ordering::Relaxed),
        CLIENTS_SNAPSHOT.load(Ordering::Relaxed),
        CLIENTS_RTSP.load(Ordering::Relaxed),
        SNAPSHOTS_TOTAL.load(Ordering::Relaxed),
    )
}

// The formatting on its own, so a test can pin the exposition shape without
// touching process-global state
fn render(webm: i64, mjpeg: i64, mkv: i64, snapshot: i64, rtsp: i64,
          snapshots_total: i64) -> String {
    format!(
        "# HELP scream_stream_clients Clients currently connected to a stream\n\
         # TYPE scream_stream_clients gauge\n\
         scream_stream_clients{{stream=\"webm\"}} {webm}\n\
         scream_stream_clients{{stream=\"mjpeg\"}} {mjpeg}\n\
         scream_stream_clients{{stream=\"mkv\"}} {mkv}\n\
         scream_stream_clients{{stream=\"snapshot\"}} {snapshot}\n\
         scream_stream_clients{{stream=\"rtsp\"}} {rtsp}\n\
         # HELP scream_snapshot_requests_total Snapshot stills served\n\
         # TYPE scream_snapshot_requests_total counter\n\
         scream_snapshot_requests_total {snapshots_total}\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_lists_every_stream_gauge() {
        let body = render(1, 2, 3, 4, 5, 6);
        assert!(body.contains("scream_stream_clients{stream=\"webm\"} 1\n"));
        assert!(body.contains("scream_stream_clients{stream=\"mjpeg\"} 2\n"));
        assert!(body.contains("scream_stream_clients{stream=\"mkv\"} 3\n"));
        assert!(body.contains("scream_stream_clients{stream=\"snapshot\"} 4\n"));
        assert!(body.contains("scream_stream_clients{stream=\"rtsp\"} 5\n"));
        assert!(body.ends_with("scream_snapshot_requests_total 6\n"));
    }

    #[test]
    fn client_guard_tracks_the_counter() {
        static COUNTER: AtomicI64 = AtomicI64::new(0);
        {
            let _a = ClientGuard::new(&COUNTER);
            let _b = ClientGuard::new(&COUNTER);
            assert_eq!(COUNTER.load(Ordering::Relaxed), 2);
        }
        assert_eq!(COUNTER.load(Ordering::Relaxed), 0);
    }
}
