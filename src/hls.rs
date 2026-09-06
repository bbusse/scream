// HLS without a disk: the mpeg-ts packets from the encoder are cut into
// segments at keyframes and kept in memory, the playlist is rendered from
// whatever is in the ring. main.rs runs the encoder and serves the two paths,
// this is the part that needs neither
//
// A segment has to be decodable on its own, so it starts with the tables a
// demuxer needs. mpegtsmux sends PAT and PMT on a timer, not at keyframes, so
// the latest of each is remembered and put in front of every new segment

use std::collections::VecDeque;
use std::time::Duration;

pub const PLAYLIST_PATH: &str = "/hls/stream.m3u8";
// Keyframe interval and so the segment length. One second keeps the join
// time and the play position near what dlna players get, the player runs
// three target durations behind the live edge
pub const SEGMENT_SECS: u32 = 1;
// A live playlist needs three segments before a player will start
pub const MIN_SEGMENTS: usize = 3;
// How many the playlist lists, and how many are kept so a segment named in
// the last playlist a player fetched is still there when it asks for it
pub const PLAYLIST_SEGMENTS: usize = 4;
pub const RING_SEGMENTS: usize = 6;

const TS_PACKET: usize = 188;
const PAT_PID: u16 = 0;

pub struct Segment {
    pub seq: u64,
    pub duration: Duration,
    pub data: Vec<u8>,
    // First segment of an encoder run: its timeline is unrelated to what a
    // player may have played before, the playlist says so
    pub discontinuity: bool,
}

// Cuts a packet stream into segments. Feed every packet in order, a finished
// segment comes back on the packet that starts the next one
pub struct Segmenter {
    pat: Vec<u8>,
    pmt: Vec<u8>,
    pmt_pid: Option<u16>,
    current: Vec<u8>,
    start: Option<Duration>,
    next_seq: u64,
    first: bool,
}

fn pid(packet: &[u8]) -> u16 {
    (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2])
}

// The PMT pid is in the PAT: after the adaptation field, which mpegtsmux
// pads a short table with, the pointer field and the 8 byte table header
// come 4 byte program entries, the pid is the low 13 bits of the last two.
// Program 0 is the network pid and skipped
fn pmt_pid_from_pat(packet: &[u8]) -> Option<u16> {
    let mut payload = 4;
    if packet[3] & 0x20 != 0 {
        payload += 1 + usize::from(packet[4]);
    }
    let pointer = *packet.get(payload)?;
    let table = packet.get(payload + 1 + usize::from(pointer)..)?;
    let section_length = usize::from(u16::from_be_bytes([table[1] & 0x0f, table[2]]));
    let programs = table.get(8..(3 + section_length).saturating_sub(4))?;
    programs.chunks_exact(4)
        .find(|p| u16::from_be_bytes([p[0], p[1]]) != 0)
        .map(|p| u16::from_be_bytes([p[2] & 0x1f, p[3]]))
}

impl Segmenter {
    pub fn new(next_seq: u64) -> Self {
        Segmenter {
            pat: Vec::new(), pmt: Vec::new(), pmt_pid: None,
            current: Vec::new(), start: None, next_seq, first: true,
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    // One buffer from the muxer, one or more whole ts packets. keyframe is
    // the buffer flag mpegtsmux sets on the packet that starts a video
    // keyframe, pts that packet's timestamp. The flag is also on the tables
    // it sends first, which are not a cut, and only one cut per timestamp
    pub fn push(&mut self, data: &[u8], keyframe: bool, pts: Duration) -> Option<Segment> {
        let mut done = None;
        let mut starts_keyframe = false;
        for packet in data.chunks_exact(TS_PACKET) {
            match pid(packet) {
                PAT_PID => {
                    self.pat.clear();
                    self.pat.extend_from_slice(packet);
                    self.pmt_pid = pmt_pid_from_pat(packet);
                }
                p if Some(p) == self.pmt_pid => {
                    self.pmt.clear();
                    self.pmt.extend_from_slice(packet);
                }
                _ => starts_keyframe |= keyframe,
            }
        }
        if starts_keyframe && self.start.is_none_or(|start| pts > start) {
            if let Some(start) = self.start {
                done = Some(self.finish(pts - start));
            }
            self.start = Some(pts);
            self.current.extend_from_slice(&self.pat);
            self.current.extend_from_slice(&self.pmt);
        }
        // Nothing before the first keyframe is decodable
        if self.start.is_some() {
            self.current.extend_from_slice(data);
        }
        done
    }

    fn finish(&mut self, duration: Duration) -> Segment {
        let seq = self.next_seq;
        self.next_seq += 1;
        let discontinuity = self.first;
        self.first = false;
        Segment { seq, duration, data: std::mem::take(&mut self.current), discontinuity }
    }
}

// The segment sequence a request names, /hls/<seq>.ts
pub fn segment_seq(path: &str) -> Option<u64> {
    path.strip_prefix("/hls/")?.strip_suffix(".ts")?.parse().ok()
}

pub fn segment_path(seq: u64) -> String {
    format!("/hls/{seq}.ts")
}

// The live playlist for the newest segments in the ring. A player treats a
// playlist without EXT-X-ENDLIST as live and refetches it
pub fn playlist(ring: &VecDeque<Segment>) -> String {
    let skip = ring.len().saturating_sub(PLAYLIST_SEGMENTS);
    let listed: Vec<&Segment> = ring.iter().skip(skip).collect();
    let target = listed.iter()
        .map(|s| s.duration.as_secs_f64().round() as u32)
        .max().unwrap_or(SEGMENT_SECS).max(SEGMENT_SECS);
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-MEDIA-SEQUENCE:{}\n",
        listed.first().map_or(0, |s| s.seq));
    for s in listed {
        if s.discontinuity {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        out.push_str(&format!("#EXTINF:{:.3},\n{}\n", s.duration.as_secs_f64(),
                              segment_path(s.seq)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(pid: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x47, (pid >> 8) as u8 | 0x40, pid as u8, 0x10];
        p.extend_from_slice(payload);
        p.resize(TS_PACKET, 0xff);
        p
    }

    fn pat_table() -> Vec<u8> {
        let mut table = vec![0x00, 0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00];
        table.extend_from_slice(&[0x00, 0x01, 0xe0, 0x20]);
        table.extend_from_slice(&[0, 0, 0, 0]);
        table
    }

    // A PAT announcing program 1 on pid 0x20
    fn pat() -> Vec<u8> {
        packet(0, &pat_table())
    }

    // The same table the way mpegtsmux sends it: pushed to the end of the
    // packet by a stuffing adaptation field
    fn pat_with_adaptation_field() -> Vec<u8> {
        let table = pat_table();
        let stuffing = TS_PACKET - 4 - 1 - table.len();
        let mut p = vec![0x47, 0x40, 0x00, 0x31, stuffing as u8, 0x00];
        p.resize(4 + 1 + stuffing, 0xff);
        p.extend_from_slice(&table);
        assert_eq!(p.len(), TS_PACKET);
        p
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn pmt_pid_is_read_from_the_pat() {
        assert_eq!(pmt_pid_from_pat(&pat()), Some(0x20));
        assert_eq!(pmt_pid_from_pat(&pat_with_adaptation_field()), Some(0x20));
    }

    #[test]
    fn segments_start_with_tables_and_cut_at_keyframes() {
        let mut seg = Segmenter::new(7);
        let pmt = packet(0x20, &[0x02]);
        let video = packet(0x41, &[1]);
        // mpegtsmux flags its first tables like a keyframe, they are no cut
        assert!(seg.push(&pat(), true, secs(0)).is_none());
        assert!(seg.push(&pmt, true, secs(0)).is_none());
        // A non-keyframe before the first keyframe is dropped
        assert!(seg.push(&video, false, secs(0)).is_none());
        assert!(seg.push(&video, true, secs(1)).is_none());
        // A second flagged packet at the same time is the same keyframe
        assert!(seg.push(&video, true, secs(1)).is_none());
        assert!(seg.push(&video, false, secs(1)).is_none());
        let first = seg.push(&video, true, secs(2)).expect("segment on next keyframe");
        assert_eq!(first.seq, 7);
        assert!(first.discontinuity);
        assert_eq!(first.duration, secs(1));
        assert_eq!(first.data.len(), 5 * TS_PACKET);
        assert_eq!(pid(&first.data[..TS_PACKET]), 0);
        assert_eq!(pid(&first.data[TS_PACKET..2 * TS_PACKET]), 0x20);
        assert_eq!(pid(&first.data[2 * TS_PACKET..3 * TS_PACKET]), 0x41);

        let second = seg.push(&video, true, secs(3)).unwrap();
        assert_eq!(second.seq, 8);
        assert!(!second.discontinuity);
        assert_eq!(second.data.len(), 3 * TS_PACKET);
        assert_eq!(seg.next_seq(), 9);
    }

    #[test]
    fn segment_paths_round_trip() {
        assert_eq!(segment_seq(&segment_path(42)), Some(42));
        assert_eq!(segment_seq("/hls/stream.m3u8"), None);
        assert_eq!(segment_seq("/hls/x.ts"), None);
        assert_eq!(segment_seq("/42.ts"), None);
    }

    #[test]
    fn playlist_lists_the_newest_segments() {
        let ring: VecDeque<Segment> = (0..6).map(|seq| Segment {
            seq, duration: Duration::from_millis(1000), data: vec![], discontinuity: seq == 0,
        }).collect();
        let m3u8 = playlist(&ring);
        assert!(m3u8.starts_with("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:2\n"));
        assert!(!m3u8.contains("/hls/1.ts"));
        assert!(m3u8.contains("#EXTINF:1.000,\n/hls/2.ts\n"));
        assert!(m3u8.ends_with("/hls/5.ts\n"));
        assert!(!m3u8.contains("DISCONTINUITY"));
        assert!(!m3u8.contains("ENDLIST"));
    }

    #[test]
    fn playlist_marks_a_new_timeline_and_rounds_target_up() {
        let ring: VecDeque<Segment> = vec![Segment {
            seq: 3, duration: Duration::from_millis(1600), data: vec![], discontinuity: true,
        }].into();
        let m3u8 = playlist(&ring);
        assert!(m3u8.contains("#EXT-X-TARGETDURATION:2\n"));
        assert!(m3u8.contains("#EXT-X-DISCONTINUITY\n#EXTINF:1.600,\n/hls/3.ts\n"));
    }
}
