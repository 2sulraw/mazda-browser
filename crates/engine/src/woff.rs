//! Font container decoding: raw sfnt + WOFF1 (`wOFF`) → raw sfnt for ab_glyph.
//! WOFF2 (`wOF2`, Brotli) decodes too when the optional `woff2` feature is
//! compiled in; feature OFF keeps the honest reject → caller falls back to
//! the next `src` URL or the site's stack/generic font.

use std::io::Read;

/// A corrupt header claiming gigabytes must not OOM us (zip-bomb guard).
/// Generous vs. real fonts (largest shipping tables are a few MB).
const MAX_TABLE_BYTES: usize = 64 << 20;
/// Same guard applied to the sum of all tables in one font.
const MAX_FONT_BYTES: usize = 256 << 20;

/// Decode font bytes in any container we support to a raw sfnt
/// (`\0\x1\0\0`/OTTO/true) parseable by the font lib.
/// `wOF2` (WOFF2/Brotli): decoded when this build has the `woff2` feature,
/// else None — the caller tries the next `src` URL and finally the site's
/// generic/system font, so a missing decoder costs typography, never a crash.
pub fn decode_font(bytes: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
    // four sfnt version magics: TrueType, CFF, True, type1
    if bytes.starts_with(&[0x00, 0x01, 0x00, 0x00])
        || bytes.starts_with(b"OTTO")
        || bytes.starts_with(b"true")
        || bytes.starts_with(b"typ1")
    {
        return Some(std::borrow::Cow::Borrowed(bytes)); // already raw sfnt
    }
    if bytes.starts_with(b"wOFF") {
        return woff1_to_sfnt(bytes).map(std::borrow::Cow::Owned);
    }
    if bytes.starts_with(b"wOF2") {
        return woff2_to_sfnt(bytes).map(std::borrow::Cow::Owned);
    }
    None // anything else — unsupported container
}

/// WOFF2 → sfnt. `wuff` (pure Rust, MIT) undoes the per-table transforms
/// (glyf/loca + hmtx) and reassembles a complete sfnt with a directory.
/// shortcut: the transform/uncompressed-table coverage is exactly what wuff
/// implements (WOFF2 1.0); any rejected or malformed font returns None, and
/// the caller falls back to system fonts rather than rendering wrong glyphs.
/// The header's `totalSfntSize` is checked before allocating/decompressing so
/// a corrupt (zip-bomb) header cannot balloon memory.
#[cfg(feature = "woff2")]
pub fn woff2_to_sfnt(data: &[u8]) -> Option<Vec<u8>> {
    // 48-byte fixed header; totalSfntSize at offset 16
    if data.len() < 48 || &data[0..4] != b"wOF2" {
        return None;
    }
    let claimed = u32::from_be_bytes(data[16..20].try_into().ok()?) as usize;
    if claimed == 0 || claimed > MAX_FONT_BYTES {
        return None;
    }
    let sfnt = wuff::decompress_woff2(data).ok()?;
    // the reassembled font must be one the font lib can read (and a sane size)
    if sfnt.len() > MAX_FONT_BYTES
        || !(sfnt.starts_with(&[0x00, 0x01, 0x00, 0x00])
            || sfnt.starts_with(b"OTTO")
            || sfnt.starts_with(b"true")
            || sfnt.starts_with(b"typ1"))
    {
        return None;
    }
    // numTables lives at 4..6; zero tables is not a usable font
    if sfnt.len() < 12 || sfnt[4..6] == [0, 0] {
        return None;
    }
    Some(sfnt)
}

/// Feature-off stub: keeps `decode_font`'s signature/dispatch unchanged and
/// makes the fallback explicit (never a silent partial decode).
#[cfg(not(feature = "woff2"))]
pub fn woff2_to_sfnt(_data: &[u8]) -> Option<Vec<u8>> {
    None
}

/// WOFF1 → sfnt per WOFF 1.0 (w3.org/TR/WOFF/): 44-byte header, 20-byte
/// directory entries (sorted by tag), zlib (RFC1950) per table.
/// Returns None on any malformed input.
/// shortcut: doesn't validate table checksums or totalSfntSize, and ignores
/// the metadata/private blocks (they're not part of the sfnt anyway).
pub fn woff1_to_sfnt(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 44 || &data[0..4] != b"wOFF" {
        return None;
    }
    let num_tables = u16::from_be_bytes(data[12..14].try_into().ok()?);
    if num_tables == 0 || 44 + 20 * num_tables as usize > data.len() {
        return None;
    }
    // collect the WOFF directory: (tag, offset, compLength, origLength, checksum)
    let mut tables: Vec<(u32, usize, usize, usize, u32)> = Vec::with_capacity(num_tables as usize);
    let mut total = 0usize;
    for i in 0..num_tables as usize {
        let e = &data[44 + 20 * i..44 + 20 * (i + 1)];
        let tag = u32::from_be_bytes(e[0..4].try_into().ok()?);
        let offset = u32::from_be_bytes(e[4..8].try_into().ok()?) as usize;
        let comp = u32::from_be_bytes(e[8..12].try_into().ok()?) as usize;
        let orig = u32::from_be_bytes(e[12..16].try_into().ok()?) as usize;
        let checksum = u32::from_be_bytes(e[16..20].try_into().ok()?);
        if offset.checked_add(comp).map_or(true, |end| end > data.len()) {
            return None;
        }
        if orig > MAX_TABLE_BYTES || total + orig > MAX_FONT_BYTES {
            return None;
        }
        total += orig;
        tables.push((tag, offset, comp, orig, checksum));
    }
    tables.sort_by_key(|t| t.0); // sfnt records must be tag-sorted (WOFF spec requires it too)

    // 12-byte sfnt offset table: searchRange = largest power of 2 ≤ n × 16
    let n = tables.len() as u16;
    let mut entry_selector = 0u16;
    while (2usize << entry_selector) <= tables.len() {
        entry_selector += 1;
    }
    let search_range = (1u16 << entry_selector) * 16;
    let range_shift = n * 16 - search_range;
    let flavor = u32::from_be_bytes(data[4..8].try_into().ok()?);

    let header_len = 12 + 16 * tables.len();
    let mut out = Vec::with_capacity(header_len + total);
    out.extend_from_slice(&flavor.to_be_bytes());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());
    // 16-byte records first, then padded table data — offsets must be 4-aligned
    let mut offset = header_len as u32;
    for &(tag, _, _comp, orig, checksum) in &tables {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&checksum.to_be_bytes()); // orig checksum, computed pre-compression — correct as-is
        out.extend_from_slice(&offset.to_be_bytes());
        out.extend_from_slice(&(orig as u32).to_be_bytes());
        offset += ((orig + 3) & !3) as u32;
    }
    for &(_tag, off, comp, orig, _) in &tables {
        let chunk = &data[off..off + comp];
        if comp == orig {
            out.extend_from_slice(chunk); // stored uncompressed
        } else {
            // zlib (RFC1950), NOT raw deflate; take() caps a zip bomb at the
            // claimed origLength, the len check catches truncated/overlong streams
            let mut raw = Vec::with_capacity(orig);
            flate2::read::ZlibDecoder::new(chunk)
                .take(orig as u64 + 1)
                .read_to_end(&mut raw)
                .ok()?;
            if raw.len() != orig {
                return None;
            }
            out.extend_from_slice(&raw);
        }
        while out.len() % 4 != 0 {
            out.push(0); // sfnt pads tables to 4-byte boundaries
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn put_u32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    /// Minimal sfnt: one dummy `cmap` table holding `content`.
    fn tiny_sfnt(content: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        put_u32(&mut s, 0x0001_0000); // sfnt version (flavor)
        s.extend_from_slice(&1u16.to_be_bytes()); // numTables
        s.extend_from_slice(&16u16.to_be_bytes()); // searchRange
        s.extend_from_slice(&0u16.to_be_bytes()); // entrySelector
        s.extend_from_slice(&0u16.to_be_bytes()); // rangeShift
        s.extend_from_slice(b"cmap"); // tag
        put_u32(&mut s, 0); // checksum (our parser doesn't validate it)
        put_u32(&mut s, 28); // offset = 12 + 16
        put_u32(&mut s, content.len() as u32); // length
        s.extend_from_slice(content);
        s
    }

    /// WOFF1 wrapping `tiny_sfnt`'s single table, optionally zlib-compressed.
    fn woff1_of(sfnt: &[u8], compress: bool) -> Vec<u8> {
        let content = &sfnt[28..];
        let payload = if compress {
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(content).unwrap();
            enc.finish().unwrap()
        } else {
            content.to_vec()
        };
        let mut w = Vec::new();
        w.extend_from_slice(b"wOFF");
        w.extend_from_slice(&sfnt[0..4]); // flavor
        put_u32(&mut w, (44 + 20 + payload.len()) as u32); // length
        w.extend_from_slice(&1u16.to_be_bytes()); // numTables
        w.extend_from_slice(&0u16.to_be_bytes()); // reserved
        put_u32(&mut w, sfnt.len() as u32); // totalSfntSize
        w.extend_from_slice(&1u16.to_be_bytes()); // majorVersion
        w.extend_from_slice(&0u16.to_be_bytes()); // minorVersion
        for _ in 0..5 {
            put_u32(&mut w, 0); // metaOffset/len/origLength, privOffset/len
        }
        w.extend_from_slice(&sfnt[12..16]); // tag
        put_u32(&mut w, 64); // offset = 44 + 20
        put_u32(&mut w, payload.len() as u32); // compLength
        put_u32(&mut w, content.len() as u32); // origLength
        put_u32(&mut w, 0); // origChecksum
        w.extend_from_slice(&payload);
        w
    }

    #[test]
    fn sfnt_magic_passes_through() {
        let sfnt = tiny_sfnt(&[9, 8, 7, 6]);
        let got = decode_font(&sfnt).expect("raw sfnt decodes");
        assert_eq!(got.as_ref(), sfnt.as_slice());
        assert!(matches!(got, std::borrow::Cow::Borrowed(_))); // no copy
        // other sfnt version magics pass too
        assert!(decode_font(b"OTTO....").is_some());
        assert!(decode_font(b"true....").is_some());
        assert!(decode_font(b"typ1....").is_some());
    }

    #[test]
    fn woff1_roundtrip() {
        let content = [1u8, 2, 3, 4];
        let sfnt = tiny_sfnt(&content);
        let out = woff1_to_sfnt(&woff1_of(&sfnt, true)).expect("woff1 decodes");
        assert_eq!(&out[0..4], &[0, 1, 0, 0], "flavor magic survives");
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 1, "numTables survives");
        // table record: checksum 0 (not validated), offset, length
        let offset = u32::from_be_bytes(out[20..24].try_into().unwrap()) as usize;
        let length = u32::from_be_bytes(out[24..28].try_into().unwrap()) as usize;
        assert_eq!(offset % 4, 0, "table offset 4-byte aligned");
        assert_eq!(length, content.len());
        assert_eq!(&out[offset..offset + length], &content, "table content intact");
        assert_eq!(out.len(), 32, "12 header + 16 record + 4 padded data");
    }

    #[test]
    fn woff1_uncompressed_table_passthrough() {
        // compLength == origLength → stored verbatim, no zlib involved
        let content = [5u8, 6, 7, 8];
        let sfnt = tiny_sfnt(&content);
        let out = woff1_to_sfnt(&woff1_of(&sfnt, false)).expect("stored woff1 decodes");
        assert_eq!(&out[28..32], &content);
    }

    #[test]
    fn woff1_bad_signature_is_none() {
        assert!(woff1_to_sfnt(b"garbage that is not a font container").is_none());
        assert!(woff1_to_sfnt(b"wOFF").is_none()); // truncated header
        assert!(woff1_to_sfnt(&woff1_of(&tiny_sfnt(&[0; 4]), true)[..43]).is_none());
    }

    /// Real 1.4 KB WOFF2 (web-platform-tests `valid-002`, CFF flavor).
    const WOFF2_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tiny.woff2");

    #[cfg(feature = "woff2")]
    #[test]
    fn woff2_fixture_decodes_to_a_complete_sfnt() {
        let sfnt = decode_font(WOFF2_FIXTURE).expect("woff2 decodes with the feature on");
        assert!(matches!(sfnt, std::borrow::Cow::Owned(_)));
        // the sfnt version is carried over from the WOFF2 header (this
        // fixture is CFF-flavored → OTTO; TrueType → \0\x01\0\0)
        assert!(sfnt.starts_with(b"OTTO"), "flavor signature preserved");
        // dispatch produced an sfnt the font parser accepts (not just a magic)
        let font = ab_glyph::FontVec::try_from_vec(sfnt.into_owned());
        assert!(font.is_ok(), "decoded WOFF2 must parse as a real font");
    }

    #[cfg(feature = "woff2")]
    #[test]
    fn woff2_garbage_and_truncation_are_none_not_panic() {
        // truncated at every boundary of the real fixture: never panics
        let n = WOFF2_FIXTURE.len();
        for cut in [0, 1, 3, 4, 12, 20, 47, 48, n / 2, n - 1] {
            assert!(decode_font(&WOFF2_FIXTURE[..cut]).is_none(), "cut {cut}");
        }
        // right magic, junk body (and a lying totalSfntSize)
        let mut lie = b"wOF2\x00\x00\x00\x00".to_vec();
        lie.extend_from_slice(&[0xff; 60]);
        lie[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_font(&lie).is_none());
        assert!(decode_font(b"wOF2").is_none());
    }

    #[cfg(not(feature = "woff2"))]
    #[test]
    fn woff2_feature_off_returns_none_honest_fallback() {
        // default (dev) build has no decoder — reject rather than render wrong
        assert!(decode_font(WOFF2_FIXTURE).is_none());
        assert!(decode_font(b"wOF2\x00\x00\x00\x01brotli bytes here").is_none());
        assert!(decode_font(b"wOF2").is_none()); // truncated header
    }

    #[test]
    fn unknown_container_is_none() {
        assert!(decode_font(b"GIF89a...").is_none());
        assert!(decode_font(b"").is_none());
        assert!(decode_font(b"plain text, not a font").is_none());
    }
}
