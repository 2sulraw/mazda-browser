//! Image decoding for `<img>`: PNG + JPEG → RGBA32.
//! shortcut: no WebP/AVIF/GIF/SVG decode (magic sniff rejects them → invisible box).

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
    } else {
        None // WebP/AVIF/GIF/SVG/… (documented gap)
    }
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

/// Intrinsic size from the container header alone — PNG IHDR / JPEG SOF.
/// No pixel decode: lets layout size the box before we pay for pixels.
pub fn probe(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
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
