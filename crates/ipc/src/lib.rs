//! Phase S IPC: line commands over stdio + a memory-mapped frame buffer.
//!
//! Commands are newline-delimited UTF-8 on stdin; events are newline-delimited
//! UTF-8 on stdout. Frames never travel through the pipe: the renderer writes
//! BGRA pixels into a memory-mapped file and announces it with a `frame` line.
//!
//! This is deliberately the smallest thing that can measure the process split.
//! The real protocol (postcard messages, double-buffered shared memory) lands
//! in P1, once Phase S says the split is worth building.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// "MZDB" — frame header magic.
pub const MAGIC: u32 = 0x4D5A_4442;
/// magic + width + height + seq, all u32 LE.
pub const HEADER_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub width: u32,
    pub height: u32,
    pub seq: u32,
}

pub fn header_bytes(h: FrameHeader) -> [u8; HEADER_LEN] {
    let mut b = [0u8; HEADER_LEN];
    b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    b[4..8].copy_from_slice(&h.width.to_le_bytes());
    b[8..12].copy_from_slice(&h.height.to_le_bytes());
    b[12..16].copy_from_slice(&h.seq.to_le_bytes());
    b
}

pub fn parse_header(b: &[u8]) -> Option<FrameHeader> {
    if b.len() < HEADER_LEN {
        return None;
    }
    let u = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    if u(0) != MAGIC {
        return None;
    }
    let width = u(4);
    let height = u(8);
    // a torn/partial mapping is not a frame
    if width == 0 || height == 0 || b.len() < HEADER_LEN + width as usize * height as usize * 4 {
        return None;
    }
    Some(FrameHeader { width, height, seq: u(12) })
}

/// Path for a frame of this size. Size is in the name so a resize never
/// remaps a buffer the other side is reading.
pub fn frame_path(dir: &Path, pid: u32, w: usize, h: usize) -> PathBuf {
    dir.join(format!("mazda-frame-{pid}-{w}x{h}.bin"))
}

/// A mapped BGRA frame buffer: 16-byte header followed by `w * h` pixels.
pub struct FrameMap {
    _file: File,
    map: memmap2::MmapMut,
    width: usize,
    height: usize,
}

impl FrameMap {
    /// Create (or truncate) the mapping. Used by the renderer.
    pub fn create(path: &Path, width: usize, height: usize) -> io::Result<Self> {
        let len = HEADER_LEN + width * height * 4;
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
        let len = map.len();
        if len < HEADER_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame map too small"));
        }
        // width/height come from the header; create() sized the file to match
        let hdr = parse_header(&map[..len])
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

    /// Publish one frame. The header is written last so a reader that races
    /// us sees either the old seq or the new one, never a half-written frame.
    pub fn write(&mut self, seq: u32, pixels: &[u32]) {
        debug_assert_eq!(pixels.len(), self.width * self.height);
        let hdr = header_bytes(FrameHeader {
            width: self.width as u32,
            height: self.height as u32,
            seq,
        });
        // pixels: 0x00RRGGBB -> BGRA bytes
        let px = &mut self.map[HEADER_LEN..HEADER_LEN + pixels.len() * 4];
        for (i, &p) in pixels.iter().enumerate() {
            let o = i * 4;
            px[o] = p as u8;
            px[o + 1] = (p >> 8) as u8;
            px[o + 2] = (p >> 16) as u8;
            px[o + 3] = 0xff;
        }
        // magic is already there from create(); rewrite the whole header last
        self.map[..HEADER_LEN].copy_from_slice(&hdr);
    }

    pub fn header(&self) -> Option<FrameHeader> {
        parse_header(&self.map[..])
    }

    pub fn pixels(&self) -> &[u8] {
        &self.map[HEADER_LEN..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = FrameHeader { width: 4, height: 4, seq: 7 };
        let mut b = vec![0u8; HEADER_LEN + 4 * 4 * 4];
        b[..HEADER_LEN].copy_from_slice(&header_bytes(h));
        assert_eq!(parse_header(&b), Some(h));
    }

    #[test]
    fn rejects_bad_magic_and_short_buffers() {
        let h = FrameHeader { width: 4, height: 4, seq: 1 };
        let mut b = vec![0u8; HEADER_LEN + 4 * 4 * 4];
        b[..HEADER_LEN].copy_from_slice(&header_bytes(h));
        assert_eq!(parse_header(&b[..8]), None);
        assert_eq!(parse_header(&b[..HEADER_LEN]), None, "header without pixels is torn");
        b[0] ^= 0xff;
        assert_eq!(parse_header(&b), None);
    }

    #[test]
    fn frame_map_round_trips_pixels() {
        let path = std::env::temp_dir().join(format!("mazda-ipc-test-{}.bin", std::process::id()));
        let mut m = FrameMap::create(&path, 3, 2).unwrap();
        let px = [0x00_11_22_33u32, 0x00_44_55_66, 0x00_77_88_99, 0, 0x00_ff_ff_ff, 0x00_00_00_01];
        m.write(9, &px);
        let h = m.header().unwrap();
        assert_eq!((h.width, h.height, h.seq), (3, 2, 9));
        // BGRA: first pixel is 33 22 11 ff
        assert_eq!(&m.pixels()[..4], &[0x33, 0x22, 0x11, 0xff]);
        drop(m);
        let _ = std::fs::remove_file(path);
    }
}
