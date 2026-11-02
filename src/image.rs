//! Image decoding for `<img>`: PNG + JPEG + WebP → RGBA32.
//! shortcut: no AVIF/GIF decode (magic sniff rejects them → invisible box).

use std::io::Cursor;

/// Decoded pixels, `0xAARRGGBB`, row-major, `px.len() == w * h`.
pub struct ImageData {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u32>,
}

/// Sniff the format from magic bytes and decode. `None` = unsupported or corrupt.
pub fn decode(bytes: &[u8]) -> Option<ImageData> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        decode_png(bytes)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        decode_jpeg(bytes)
    } else if is_webp(bytes) {
        decode_webp(bytes)
    } else {
        None // AVIF/GIF/… (documented gap)
    }
}

/// `RIFF....WEBP` — the container tag is at bytes 8..12, not a prefix.
fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

fn decode_webp(bytes: &[u8]) -> Option<ImageData> {
    let mut dec = image_webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
    let (w, h) = dec.dimensions();
    if w == 0 || h == 0 || (w as usize) * (h as usize) > 32_000_000 {
        return None;
    }
    // shortcut: animated WebP has no timeline here — only frame 0 is drawn.
    if dec.is_animated() {
        eprintln!("[img] animated webp: first of {} frames", dec.num_frames());
    }
    let n = (w * h) as usize;
    let mut buf = vec![0u8; dec.output_buffer_size()?];
    dec.read_image(&mut buf).ok()?;
    let mut px = Vec::with_capacity(n);
    if dec.has_alpha() {
        // lossless alpha is exact; lossy images carry a separate lossless
        // alpha plane (ALPH), so both kinds keep their transparency here.
        for p in buf.chunks_exact(4) {
            px.push((p[3] as u32) << 24 | (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32);
        }
    } else {
        for p in buf.chunks_exact(3) {
            px.push(0xff00_0000 | (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32);
        }
    }
    (px.len() == n).then_some(ImageData { w, h, px })
}

fn decode_png(bytes: &[u8]) -> Option<ImageData> {
    let mut dec = png::Decoder::new(bytes);
    // palette → rgb, tRNS → alpha, <8bit gray → 8bit, 16bit → 8bit
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || (w as usize) * (h as usize) > 32_000_000 {
        return None;
    }
    let stride = info.line_size;
    let ch = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None, // EXPAND should prevent this
    };
    let mut px = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        let row = &buf[y * stride..];
        for x in 0..w as usize {
            let p = &row[x * ch..];
            let (r, g, b, a) = match ch {
                1 => (p[0], p[0], p[0], 255),
                2 => (p[0], p[0], p[0], p[1]),
                3 => (p[0], p[1], p[2], 255),
                _ => (p[0], p[1], p[2], p[3]),
            };
            px.push((a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32);
        }
    }
    Some(ImageData { w, h, px })
}

fn decode_jpeg(bytes: &[u8]) -> Option<ImageData> {
    let mut dec = jpeg_decoder::Decoder::new(bytes);
    let raw = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    if w == 0 || h == 0 || (w as usize) * (h as usize) > 32_000_000 {
        return None;
    }
    let n = (w * h) as usize;
    let mut px = Vec::with_capacity(n);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            for p in raw.chunks_exact(3) {
                px.push(0xff00_0000 | (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32);
            }
        }
        _ => return None, // CMYK/other: shortcut — treated as unsupported
    }
    if px.len() != n {
        return None;
    }
    Some(ImageData { w, h, px })
}

/// Intrinsic size from the container header alone — PNG IHDR / JPEG SOF /
/// WebP VP8|VP8L|VP8X header. No pixel decode: lets layout size the box
/// before we pay for pixels.
pub fn probe(bytes: &[u8]) -> Option<(u32, u32)> {    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        if bytes.len() < 24 {
            return None;
        }
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return (w > 0 && h > 0).then_some((w, h));
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        let mut i = 2;
        while i + 9 < bytes.len() {
            if bytes[i] != 0xff {
                i += 1;
                continue;
            }
            let m = bytes[i + 1];
            if m == 0xd8 || m == 0x01 || (0xd0..=0xd7).contains(&m) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
            // SOF0-3/5-7/9-11/13-15 carry the frame dimensions
            if matches!(m, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
                let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
                return (w > 0 && h > 0).then_some((w, h));
            }
            if len < 2 {
                return None;
            }
            i += 2 + len;
        }
    }
    if is_webp(bytes) {
        // Header-only: parses RIFF + the VP8/VP8L/VP8X headers (dimensions,
        // alpha/animation flags), never the pixel bitstream.
        let dec = image_webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
        let (w, h) = dec.dimensions();
        return (w > 0 && h > 0).then_some((w, h));
    }
    None
}

/// Nearest-neighbour resample to the display size — run after layout so big
/// intrinsic buffers (multi-MB hero photos) are dropped from RAM.
pub fn downscale(src: &ImageData, w: u32, h: u32) -> ImageData {
    let w = w.max(1);
    let h = h.max(1);
    let mut px = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let v = (y as u64 * src.h as u64) / h as u64;
        for x in 0..w {
            let u = (x as u64 * src.w as u64) / w as u64;
            px.push(src.px[(v * src.w as u64 + u) as usize]);
        }
    }
    ImageData { w, h, px }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_png() -> Vec<u8> {
        // encode 2x1 RGBA: red@50% alpha, opaque green
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 2, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 128, 0, 255, 0, 255]).unwrap();
        }
        bytes
    }

    #[test]
    fn png_roundtrip_preserves_alpha() {
        let img = decode(&tiny_png()).expect("png decodes");
        assert_eq!((img.w, img.h), (2, 1));
        assert_eq!(img.px[0], 0x80ff_0000);
        assert_eq!(img.px[1], 0xff00_ff00);
    }

    #[test]
    fn unknown_and_truncated_formats_rejected() {
        assert!(decode(b"GIF89a...").is_none());
        assert!(decode(b"<svg xmlns=...").is_none());
        assert!(decode(&[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10]).is_none()); // truncated jpeg
        assert!(decode(b"").is_none());
    }

    // WebP fixtures, all < 200 bytes: (1) 3x2 lossless RGBA,
    // (2) 8x6 lossy VP8X with an ALPH plane (pixel 0 is 50% transparent),
    // (3) 2x2 two-frame lossless animation (frame 0 is opaque red).
    const WEBP_LOSSLESS: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x3c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x4c,
        0x2f, 0x00, 0x00, 0x00, 0x2f, 0x02, 0x40, 0x00, 0x10, 0x2f, 0x20, 0x10, 0x48, 0xda, 0x1f, 0x7a,
        0x0d, 0x01, 0x41, 0x91, 0xff, 0xa3, 0x09, 0x08, 0x8a, 0xfc, 0x1f, 0x8d, 0x20, 0xdb, 0xa6, 0x68,
        0xbb, 0x3f, 0xd4, 0x03, 0xe6, 0x86, 0x95, 0xb9, 0x40, 0x20, 0x40, 0x99, 0x2a, 0x5c, 0x10, 0xd1,
        0xff, 0xf8, 0x52, 0x00,
    ];
    const WEBP_LOSSY_ALPHA: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x64, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x58,
        0x0a, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x05, 0x00, 0x00, 0x41, 0x4c,
        0x50, 0x48, 0x0d, 0x00, 0x00, 0x00, 0x01, 0x0f, 0xf0, 0xc0, 0xff, 0x88, 0x88, 0x30, 0x01, 0x11,
        0xfd, 0x8f, 0x00, 0x00, 0x56, 0x50, 0x38, 0x20, 0x30, 0x00, 0x00, 0x00, 0x90, 0x01, 0x00, 0x9d,
        0x01, 0x2a, 0x08, 0x00, 0x06, 0x00, 0x01, 0x40, 0x26, 0x25, 0xb0, 0x02, 0x74, 0xba, 0x00, 0x03,
        0x98, 0x00, 0xfe, 0xf8, 0xb5, 0xeb, 0xe6, 0x28, 0x0b, 0xff, 0xee, 0xc9, 0xff, 0xf7, 0x19, 0xff,
        0xf7, 0x19, 0xff, 0xbf, 0x6f, 0xeb, 0x53, 0x23, 0x1f, 0x53, 0xc0, 0x00,
    ];
    const WEBP_ANIM: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, 0x84, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x58,
        0x0a, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x41, 0x4e,
        0x49, 0x4d, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0x4e, 0x4d, 0x46,
        0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x64, 0x00, 0x00, 0x02, 0x56, 0x50, 0x38, 0x4c, 0x0f, 0x00, 0x00, 0x00, 0x2f, 0x01, 0x40, 0x00,
        0x00, 0x07, 0x10, 0xfd, 0x8f, 0xfe, 0x07, 0x22, 0xa2, 0xff, 0x01, 0x00, 0x41, 0x4e, 0x4d, 0x46,
        0x28, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x64, 0x00, 0x00, 0x00, 0x56, 0x50, 0x38, 0x4c, 0x0f, 0x00, 0x00, 0x00, 0x2f, 0x01, 0x40, 0x00,
        0x00, 0x07, 0xd0, 0xff, 0x88, 0xfe, 0x07, 0x22, 0xa2, 0xff, 0x01, 0x00,
    ];

    #[test]
    fn webp_magic_detected_and_near_misses_rejected() {
        assert!(is_webp(WEBP_LOSSLESS));
        assert!(is_webp(WEBP_LOSSY_ALPHA));
        // valid tags, swapped container ids → not WebP
        let mut rifx = WEBP_LOSSLESS.to_vec();
        rifx[..4].copy_from_slice(b"RIFX");
        let mut webq = WEBP_LOSSLESS.to_vec();
        webq[8..12].copy_from_slice(b"WEBQ");
        assert!(!is_webp(&rifx));
        assert!(!is_webp(&webq));
        assert!(decode(&rifx).is_none());
        assert!(decode(&webq).is_none());
        assert!(!is_webp(b"RIFF"));
    }

    #[test]
    fn webp_probe_reads_dimensions_from_header() {
        assert_eq!(probe(WEBP_LOSSLESS), Some((3, 2)));
        assert_eq!(probe(WEBP_LOSSY_ALPHA), Some((8, 6)));
        assert_eq!(probe(WEBP_ANIM), Some((2, 2)));
    }

    #[test]
    fn webp_lossless_decode_preserves_alpha() {
        let img = decode(WEBP_LOSSLESS).expect("lossless webp decodes");
        assert_eq!((img.w, img.h), (3, 2));
        assert_eq!(img.px.len(), 6);
        assert_eq!(img.px[0], 0x80ff_0000); // red @ 50%
        assert_eq!(img.px[1], 0xff00_ff00); // opaque green
        assert_eq!(img.px[2], 0x4000_00ff); // blue @ 25%
        assert_eq!(img.px[4], 0x0000_0000); // fully transparent black
        assert_eq!(img.px[5], 0xffff_ffff);
    }

    #[test]
    fn webp_lossy_with_alpha_plane_decodes() {
        let img = decode(WEBP_LOSSY_ALPHA).expect("lossy webp decodes");
        assert_eq!((img.w, img.h), (8, 6));
        assert_eq!(img.px.len(), 48);
        assert_eq!(img.px[0] >> 24, 128); // ALPH plane, lossless
        assert_eq!(img.px[1] >> 24, 255);
    }

    #[test]
    fn webp_animated_decodes_first_frame() {
        let img = decode(WEBP_ANIM).expect("animated webp decodes");
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.px.len(), 4);
        assert_eq!(img.px[0], 0xffff_0000); // frame 0: opaque red
    }

    #[test]
    fn webp_truncated_and_garbage_rejected() {
        for cut in [4, 12, 13, 20, 30] {
            assert!(decode(&WEBP_LOSSLESS[..cut]).is_none(), "cut at {cut}");
        }
        // header-only probe needs the 25-byte VP8L header; payload truncation
        // is fine for sizing but decode still fails
        for cut in [4, 12, 13, 20] {
            assert!(probe(&WEBP_LOSSLESS[..cut]).is_none(), "probe cut at {cut}");
        }
        assert_eq!(probe(&WEBP_LOSSLESS[..30]), Some((3, 2)));
        // plausible container, corrupt payload (flip bytes past the header)
        let mut bad = WEBP_LOSSLESS.to_vec();
        for b in &mut bad[20..] {
            *b ^= 0xa5;
        }
        assert!(decode(&bad).is_none());
        assert!(decode(b"RIFF\x10\x00\x00\x00WEBP\x00\x00\x00\x00").is_none());
    }

    #[test]
    fn downscale_samples_by_area() {
        // 2x2 checkerboard → 1x1: nearest picks one source pixel (top-left)
        let src = ImageData {
            w: 2,
            h: 2,
            px: vec![0xff00_0000, 0xff00_ff00, 0xff00_00ff, 0xffff_ffff],
        };
        let d = downscale(&src, 1, 1);
        assert_eq!((d.w, d.h), (1, 1));
        assert_eq!(d.px, vec![0xff00_0000]);
        let d2 = downscale(&src, 2, 2); // same size = no-op
        assert_eq!(d2.px, src.px);
    }
}
