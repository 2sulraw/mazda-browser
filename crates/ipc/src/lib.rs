//! P1 IPC: typed line messages over stdio + double-buffered shared-memory frames.
//!
//! Commands are newline-delimited UTF-8 on stdin, events are newline-delimited
//! UTF-8 on stdout. The wire format is still human-readable on purpose: it is
//! easy to log, easy to drive from a test, and costs nothing measurable.
//!
//! Frames never travel through the pipe. The renderer owns a memory-mapped
//! file holding a 32-byte header and two BGRA planes. It always paints into
//! the plane the reader is *not* using, then publishes the header last, so a
//! reader can copy a frame while the next one is being drawn.
//!
//! The header is a seqlock: `seq` changes only after the pixels of the newly
//! published slot are in place. A reader copies the slot named by the header
//! and re-reads the header; if `seq` moved, it retries instead of uploading a
//! torn frame.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// "MZDB" — frame header magic.
pub const MAGIC: u32 = 0x4D5A_4442;
/// Bumped when the header layout changes.
pub const VERSION: u32 = 2;
/// magic + version + width + height + seq + slot + reserved.
pub const HEADER_LEN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub width: u32,
    pub height: u32,
    pub seq: u32,
    /// plane currently holding `seq` (0 or 1)
    pub slot: u32,
}

pub fn header_bytes(h: FrameHeader) -> [u8; HEADER_LEN] {
    let mut b = [0u8; HEADER_LEN];
    b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    b[4..8].copy_from_slice(&VERSION.to_le_bytes());
    b[8..12].copy_from_slice(&h.width.to_le_bytes());
    b[12..16].copy_from_slice(&h.height.to_le_bytes());
    b[16..20].copy_from_slice(&h.seq.to_le_bytes());
    b[20..24].copy_from_slice(&h.slot.to_le_bytes());
    b
}

pub fn parse_header(b: &[u8]) -> Option<FrameHeader> {
    if b.len() < HEADER_LEN {
        return None;
    }
    let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    if u(0) != MAGIC || u(4) != VERSION {
        return None;
    }
    let width = u(8);
    let height = u(12);
    let slot = u(20);
    // a torn/partial mapping is not a frame
    if width == 0
        || height == 0
        || slot > 1
        || b.len() < HEADER_LEN + 2 * width as usize * height as usize * 4
    {
        return None;
    }
    Some(FrameHeader { width, height, seq: u(16), slot })
}

/// Path for a frame of this size. Size is in the name so a resize never
/// remaps a buffer the other side is reading.
pub fn frame_path(dir: &Path, pid: u32, w: usize, h: usize) -> PathBuf {
    dir.join(format!("mazda-frame-{pid}-{w}x{h}.bin"))
}

/// A mapped frame buffer: 32-byte header followed by two BGRA planes.
pub struct FrameMap {
    _file: File,
    map: memmap2::MmapMut,
    width: usize,
    height: usize,
}

impl FrameMap {
    fn plane_len(width: usize, height: usize) -> usize {
        width * height * 4
    }

    /// Create (or truncate) the mapping. Used by the renderer.
    pub fn create(path: &Path, width: usize, height: usize) -> io::Result<Self> {
        let len = HEADER_LEN + 2 * Self::plane_len(width, height);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(len as u64)?;
        // SAFETY: the file is private to this renderer process, its length is
        // set before mapping, and no other thread touches the mapping.
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        // publish a valid (empty) header before the shell can open the map
        map[..HEADER_LEN].copy_from_slice(&header_bytes(FrameHeader {
            width: width as u32,
            height: height as u32,
            seq: 0,
            slot: 0,
        }));
        Ok(Self { _file: file, map, width, height })
    }

    /// Open an existing mapping for reading. Used by the shell.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        // SAFETY: memmap2 requires the caller to guarantee the file is not
        // mutated in a way that invalidates the map while it is alive. The
        // renderer only writes pixels (never truncates) and the shell only
        // reads them, so the length is stable for the map's lifetime.
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        let hdr = parse_header(&map[..])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad frame header"))?;
        Ok(Self {
            _file: file,
            map,
            width: hdr.width as usize,
            height: hdr.height as usize,
        })
    }

    pub fn dims(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Publish one frame into the plane the reader is not using.
    ///
    /// The header is written last, and only after every pixel is in place, so
    /// a concurrent reader either keeps the previous `seq` or sees the new
    /// one — never a half-drawn frame.
    pub fn write(&mut self, seq: u32, pixels: &[u32]) {
        debug_assert_eq!(pixels.len(), self.width * self.height);
        let prev = self.header().map(|h| h.slot).unwrap_or(0);
        let slot = 1 - prev;
        let plane = HEADER_LEN + slot as usize * Self::plane_len(self.width, self.height);
        // pixels: 0x00RRGGBB -> BGRA bytes
        let px = &mut self.map[plane..plane + pixels.len() * 4];
        for (i, &p) in pixels.iter().enumerate() {
            let o = i * 4;
            px[o] = p as u8;
            px[o + 1] = (p >> 8) as u8;
            px[o + 2] = (p >> 16) as u8;
            px[o + 3] = 0xff;
        }
        self.map[..HEADER_LEN].copy_from_slice(&header_bytes(FrameHeader {
            width: self.width as u32,
            height: self.height as u32,
            seq,
            slot,
        }));
    }

    pub fn header(&self) -> Option<FrameHeader> {
        parse_header(&self.map[..])
    }

    fn active(&self) -> Option<&[u8]> {
        let hdr = self.header()?;
        let plane = HEADER_LEN + hdr.slot as usize * Self::plane_len(self.width, self.height);
        Some(&self.map[plane..plane + Self::plane_len(self.width, self.height)])
    }

    /// Copy the published frame into `out` as RGBA (what the UI wants).
    ///
    /// Returns the header that was copied. Retries when the writer publishes
    /// a newer frame mid-copy, so `out` never mixes two frames.
    pub fn copy_rgba(&self, out: &mut Vec<u8>) -> Option<FrameHeader> {
        for _ in 0..3 {
            let hdr = self.header()?;
            let src = self.active()?;
            out.clear();
            out.reserve(src.len());
            for p in src.chunks_exact(4) {
                out.extend_from_slice(&[p[2], p[1], p[0], 0xff]);
            }
            let after = self.header()?;
            if after.seq == hdr.seq && after.slot == hdr.slot {
                return Some(hdr);
            }
        }
        None
    }

    /// The published plane, BGRA. Tests and diagnostics only.
    pub fn pixels(&self) -> &[u8] {
        self.active().unwrap_or(&[])
    }
}

/// Shell → renderer.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// fresh navigation: fetch, parse, lay out
    Load(String),
    /// navigate, restoring the DOM snapshot when one exists
    Restore(String),
    Resize { width: u32, height: u32 },
    Scroll(f32),
    Click { x: f32, y: f32 },
    Quit,
}

impl Command {
    pub fn encode(&self) -> String {
        match self {
            Command::Load(url) => format!("load {url}"),
            Command::Restore(url) => format!("restore {url}"),
            Command::Resize { width, height } => format!("resize {width} {height}"),
            Command::Scroll(y) => format!("scroll {y:.1}"),
            Command::Click { x, y } => format!("click {x:.1} {y:.1}"),
            Command::Quit => "quit".to_string(),
        }
    }

    /// Parse one line. The URL is the rest of the line, so `data:` URLs with
    /// spaces survive.
    pub fn decode(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim_start();
        match head {
            "load" if !rest.is_empty() => Some(Command::Load(rest.to_string())),
            "restore" if !rest.is_empty() => Some(Command::Restore(rest.to_string())),
            "resize" => {
                let (w, h) = rest.split_once(' ')?;
                Some(Command::Resize {
                    width: w.trim().parse().ok()?,
                    height: h.trim().parse().ok()?,
                })
            }
            "scroll" => rest.trim().parse().ok().map(Command::Scroll),
            "click" => {
                let (x, y) = rest.split_once(' ')?;
                Some(Command::Click {
                    x: x.trim().parse().ok()?,
                    y: y.trim().parse().ok()?,
                })
            }
            "quit" => Some(Command::Quit),
            _ => None,
        }
    }
}

/// Renderer → shell.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// a frame was published into the mapped file
    Frame {
        path: PathBuf,
        width: usize,
        height: usize,
        doc_height: f32,
        seq: u32,
    },
    Title(String),
    Nav(String),
    Error(String),
    /// a download is streaming to disk: bytes so far and total (0 = unknown)
    Download { done: u64, total: u64, name: String },
    /// the file is complete in the renderer's temp dir; the shell moves it out
    DownloadDone { path: PathBuf },
    DownloadFailed { name: String, err: String },
    /// not on the wire: the reader thread's EOF signal
    Exited,
}

impl Event {
    pub fn encode(&self) -> String {
        match self {
            Event::Frame { path, width, height, doc_height, seq } => format!(
                "frame {} {width} {height} {doc_height:.2} {seq}",
                path.display()
            ),
            Event::Title(t) => format!("title {t}"),
            Event::Nav(u) => format!("nav {u}"),
            Event::Error(e) => format!("error {e}"),
            Event::Download { done, total, name } => {
                format!("download {done} {total} {name}")
            }
            Event::DownloadDone { path } => format!("download-done {}", path.display()),
            Event::DownloadFailed { name, err } => format!("download-failed {name}\t{err}"),
            Event::Exited => "exited".to_string(),
        }
    }

    pub fn decode(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        let (head, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim_start();
        match head {
            "frame" => {
                let mut it = rest.splitn(5, ' ');
                let path = it.next()?.to_string();
                let width = it.next()?.parse().ok()?;
                let height = it.next()?.parse().ok()?;
                let doc_height = it.next()?.parse().ok()?;
                let seq = it.next()?.parse().ok()?;
                Some(Event::Frame {
                    path: PathBuf::from(path),
                    width,
                    height,
                    doc_height,
                    seq,
                })
            }
            "title" => Some(Event::Title(rest.to_string())),
            "nav" => Some(Event::Nav(rest.to_string())),
            "error" => Some(Event::Error(rest.to_string())),
            "download" => {
                let mut it = rest.splitn(3, ' ');
                let done = it.next()?.parse().ok()?;
                let total = it.next()?.parse().ok()?;
                Some(Event::Download {
                    done,
                    total,
                    name: it.next().unwrap_or_default().to_string(),
                })
            }
            "download-done" => Some(Event::DownloadDone {
                path: PathBuf::from(rest),
            }),
            "download-failed" => {
                let (name, err) = rest.split_once('\t').unwrap_or((rest, ""));
                Some(Event::DownloadFailed {
                    name: name.to_string(),
                    err: err.to_string(),
                })
            }
            "exited" => Some(Event::Exited),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = FrameHeader { width: 4, height: 4, seq: 7, slot: 1 };
        let mut b = vec![0u8; HEADER_LEN + 2 * 4 * 4 * 4];
        b[..HEADER_LEN].copy_from_slice(&header_bytes(h));
        assert_eq!(parse_header(&b), Some(h));
    }

    #[test]
    fn rejects_bad_magic_version_and_short_buffers() {
        let h = FrameHeader { width: 4, height: 4, seq: 1, slot: 0 };
        let mut b = vec![0u8; HEADER_LEN + 2 * 4 * 4 * 4];
        b[..HEADER_LEN].copy_from_slice(&header_bytes(h));
        assert_eq!(parse_header(&b[..8]), None);
        assert_eq!(parse_header(&b[..HEADER_LEN]), None, "header without planes is torn");
        let mut bad = b.clone();
        bad[0] ^= 0xff;
        assert_eq!(parse_header(&bad), None);
        let mut old = b;
        old[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(parse_header(&old), None, "old version is not a frame");
    }

    #[test]
    fn frame_map_flips_planes_and_round_trips_pixels() {
        let path = std::env::temp_dir().join(format!("mazda-ipc-test-{}.bin", std::process::id()));
        let mut m = FrameMap::create(&path, 3, 2).unwrap();
        let px = [0x00_11_22_33u32, 0x00_44_55_66, 0x00_77_88_99, 0, 0x00_ff_ff_ff, 0x00_00_00_01];
        m.write(9, &px);
        let h = m.header().unwrap();
        assert_eq!((h.width, h.height, h.seq, h.slot), (3, 2, 9, 1));
        // BGRA: first pixel is 33 22 11 ff
        assert_eq!(&m.pixels()[..4], &[0x33, 0x22, 0x11, 0xff]);

        // the next frame goes to the other plane and RGBA is swapped back
        let px2 = [0x00_aa_bb_ccu32; 6];
        m.write(10, &px2);
        let h2 = m.header().unwrap();
        assert_eq!((h2.seq, h2.slot), (10, 0));
        let mut rgba = Vec::new();
        let copied = m.copy_rgba(&mut rgba).unwrap();
        assert_eq!((copied.seq, copied.slot), (10, 0));
        assert_eq!(&rgba[..4], &[0xaa, 0xbb, 0xcc, 0xff]);
        drop(m);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn command_round_trips() {
        let cases = [
            Command::Load("https://example.com/a b".into()),
            Command::Restore("https://example.com".into()),
            Command::Resize { width: 1000, height: 700 },
            Command::Scroll(123.4),
            Command::Click { x: 10.5, y: 20.5 },
            Command::Quit,
        ];
        for c in cases {
            let wire = c.encode();
            assert_eq!(Command::decode(&wire), Some(c), "wire: {wire}");
        }
        assert_eq!(Command::decode("load"), None, "load needs a URL");
        assert_eq!(Command::decode("resize 1000"), None);
        assert_eq!(Command::decode("nope"), None);
    }

    #[test]
    fn event_round_trips() {
        let cases = [
            Event::Frame {
                path: PathBuf::from("/tmp/f.bin"),
                width: 1000,
                height: 670,
                doc_height: 12220.0,
                seq: 7,
            },
            Event::Title("https://example.com".into()),
            Event::Nav("https://example.com/next".into()),
            Event::Error("pipeline panic: bad box".into()),
            Event::Download {
                done: 1024,
                total: 4096,
                name: "Big File (1).zip".into(),
            },
            Event::DownloadDone {
                path: PathBuf::from("C:/Temp/mazda-dl/a b.mp4"),
            },
            Event::DownloadFailed {
                name: "a b.mp4".into(),
                err: "connection reset".into(),
            },
            Event::Exited,
        ];
        for e in cases {
            let wire = e.encode();
            assert_eq!(Event::decode(&wire), Some(e), "wire: {wire}");
        }
        assert_eq!(Event::decode("frame /tmp/f.bin 1 2"), None);
    }
}
