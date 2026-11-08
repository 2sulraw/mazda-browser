//! Minimal ISO-BMFF (MP4) header reader: track kinds, display size, duration,
//! and whether the file is encrypted.
//!
//! This is deliberately not a demuxer — Media Foundation does the decoding.
//! The engine only needs enough to size the `<video>` box before a decoder
//! exists, and to reject DRM cleanly.

pub struct Mp4Meta {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub has_video: bool,
    pub has_audio: bool,
    pub drm: bool,
}

struct BoxIter<'a> {
    rest: &'a [u8],
}

struct Bx<'a> {
    kind: [u8; 4],
    body: &'a [u8],
}

impl<'a> BoxIter<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = Bx<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.rest.len() >= 8 {
            let size = u32::from_be_bytes(self.rest[0..4].try_into().ok()?) as usize;
            let kind: [u8; 4] = self.rest[4..8].try_into().ok()?;
            let (size, header) = match size {
                1 => {
                    if self.rest.len() < 16 {
                        return None;
                    }
                    let large = u64::from_be_bytes(self.rest[8..16].try_into().ok()?) as usize;
                    (large, 16)
                }
                0 => (self.rest.len(), 8),
                n => (n, 8),
            };
            if size < header || size > self.rest.len() {
                return None;
            }
            let body = &self.rest[header..size];
            self.rest = &self.rest[size..];
            // skip free/skip padding without yielding
            if &kind != b"free" && &kind != b"skip" {
                return Some(Bx { kind, body });
            }
        }
        None
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

pub fn parse(bytes: &[u8]) -> Option<Mp4Meta> {
    let mut is_mp4 = false;
    let mut moov = None;
    for b in BoxIter::new(bytes) {
        match &b.kind {
            b"ftyp" => is_mp4 = true,
            b"moov" => moov = Some(b.body),
            _ => {}
        }
    }
    if !is_mp4 {
        return None;
    }
    let moov = moov?;

    let mut meta = Mp4Meta {
        width: 0,
        height: 0,
        duration: 0.0,
        has_video: false,
        has_audio: false,
        drm: false,
    };

    // Encryption boxes live at several depths (`pssh` in `moov`, `sinf` and
    // `encv`/`enca` inside `stsd` sample entries), and the sample-entry
    // layout is not box-like, so scan the (small) `moov` payload instead of
    // walking it.
    meta.drm = [&b"pssh"[..], b"sinf", b"encv", b"enca"]
        .iter()
        .any(|k| moov.windows(k.len()).any(|w| w == *k));

    for b in BoxIter::new(moov) {
        match &b.kind {
            b"mvhd" => read_mvhd(b.body, &mut meta),
            b"trak" => read_trak(b.body, &mut meta),
            _ => {}
        }
    }
    Some(meta)
}

fn read_mvhd(b: &[u8], meta: &mut Mp4Meta) {
    let version = b.first().copied().unwrap_or(0);
    let (timescale, duration) = if version == 1 {
        (be32(b, 20), be64(b, 24))
    } else {
        (be32(b, 12), be32(b, 16).map(u64::from))
    };
    if let (Some(ts), Some(d)) = (timescale, duration) {
        if ts > 0 {
            meta.duration = d as f64 / ts as f64;
        }
    }
}

fn read_trak(b: &[u8], meta: &mut Mp4Meta) {
    let mut handler = [0u8; 4];
    let mut dims: Option<(u32, u32)> = None;
    for c in BoxIter::new(b) {
        match &c.kind {
            b"tkhd" => {
                if c.body.len() >= 8 {
                    let w = be32(c.body, c.body.len() - 8).unwrap_or(0);
                    let h = be32(c.body, c.body.len() - 4).unwrap_or(0);
                    // 16.16 fixed-point; 0 means "not set"
                    let (w, h) = (w >> 16, h >> 16);
                    if w > 0 && h > 0 {
                        dims = Some((w, h));
                    }
                }
            }
            b"mdia" => {
                // the handler sits one level down, in `trak/mdia/hdlr`
                for d in BoxIter::new(c.body) {
                    if &d.kind == b"hdlr" && d.body.len() >= 12 {
                        handler.copy_from_slice(&d.body[8..12]);
                    }
                }
            }
            _ => {}
        }
    }
    match &handler {
        b"vide" => {
            meta.has_video = true;
            if let Some((w, h)) = dims {
                if meta.width == 0 || w > meta.width {
                    meta.width = w;
                    meta.height = h;
                }
            }
        }
        b"soun" => meta.has_audio = true,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a box: 32-bit size + kind + body.
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn mvhd(version: u8, timescale: u32, duration: u64) -> Vec<u8> {
        let mut b = vec![version, 0, 0, 0];
        if version == 1 {
            b.extend_from_slice(&[0u8; 16]); // creation + modification
            b.extend_from_slice(&timescale.to_be_bytes());
            b.extend_from_slice(&duration.to_be_bytes());
        } else {
            b.extend_from_slice(&[0u8; 8]);
            b.extend_from_slice(&timescale.to_be_bytes());
            b.extend_from_slice(&(duration as u32).to_be_bytes());
        }
        b.extend_from_slice(&[0u8; 80]); // rest of the fixed fields
        bx(b"mvhd", &b)
    }

    fn tkhd(w: u32, h: u32) -> Vec<u8> {
        let mut b = vec![0u8; 76];
        b.extend_from_slice(&(w << 16).to_be_bytes());
        b.extend_from_slice(&(h << 16).to_be_bytes());
        bx(b"tkhd", &b)
    }

    fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b.extend_from_slice(kind);
        b.extend_from_slice(&[0u8; 12]);
        b.extend_from_slice(b"handler\0");
        bx(b"hdlr", &b)
    }

    fn mp4(moov_children: &[Vec<u8>], encrypted: bool) -> Vec<u8> {
        let mut moov = Vec::new();
        for c in moov_children {
            moov.extend_from_slice(c);
        }
        if encrypted {
            moov.extend_from_slice(&bx(b"pssh", &[0u8; 16]));
        }
        let mut out = bx(b"ftyp", b"isom\0\0\0\0isom");
        out.extend_from_slice(&bx(b"moov", &moov));
        out
    }

    #[test]
    fn reads_video_size_and_duration() {
        let mut trak = Vec::new();
        trak.extend_from_slice(&tkhd(640, 360));
        trak.extend_from_slice(&bx(b"mdia", &hdlr(b"vide")));
        let f = mp4(&[mvhd(0, 1000, 2500), bx(b"trak", &trak)], false);
        let m = parse(&f).expect("valid mp4");
        assert_eq!((m.width, m.height), (640, 360));
        assert!(m.has_video);
        assert!(!m.has_audio);
        assert!(!m.drm);
        assert!((m.duration - 2.5).abs() < 1e-6);
    }

    #[test]
    fn reads_audio_track_and_v1_duration() {
        let mut trak = Vec::new();
        trak.extend_from_slice(&bx(b"mdia", &hdlr(b"soun")));
        let f = mp4(&[mvhd(1, 48000, 96000), bx(b"trak", &trak)], false);
        let m = parse(&f).unwrap();
        assert!(m.has_audio);
        assert!(!m.has_video);
        assert!((m.duration - 2.0).abs() < 1e-6);
    }

    #[test]
    fn encryption_is_reported() {
        let mut trak = Vec::new();
        trak.extend_from_slice(&tkhd(320, 180));
        trak.extend_from_slice(&bx(b"mdia", &hdlr(b"vide")));
        let f = mp4(&[mvhd(0, 1000, 1000), bx(b"trak", &trak)], true);
        assert!(parse(&f).unwrap().drm);
    }

    #[test]
    fn non_mp4_is_rejected() {
        assert!(parse(b"\x89PNG\r\n\x1a\n...").is_none());
    }
}
