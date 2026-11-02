//! Responsive image source selection: `<img srcset>` and `<picture><source>`.
//!
//! Selection runs at box-build time (`layout::build`), so the URL that lands
//! in `ImgSlot::Pending` IS the chosen candidate: probe, decode and the disk
//! cache all see the one URL, and nothing re-parses the markup later.
//!
//! Selection rule (v1):
//! * `<picture>`: the first child `<source>` whose `type` we can decode
//!   (`image/webp|jpeg|png|svg+xml`, or no `type` at all) and whose `media`
//!   query matches wins; its `srcset` then picks the URL. If no source
//!   matches, the `<img>` fallback is used (`srcset` first, then `src`).
//! * `srcset` with `w` descriptors: the smallest candidate at least as wide
//!   as the slot width -- the first trivially-parseable length in `sizes`
//!   when present, else the viewport width. If nothing is wide enough, the
//!   largest candidate wins.
//! * `srcset` with `x` (or no) descriptors: the 1x candidate (the density
//!   nearest 1x when no exactly-1x entry exists).
//!
//! shortcut: viewport width only -- no DPR, no arithmetic over `sizes` media
//! conditions (only its first length token), no `loading=lazy` scheduling,
//! no `imagesrcset`, no client-hint negotiation.

use crate::html::Dom;

/// One `srcset` candidate: URL plus its width (`w`) or density (`x`)
/// descriptor. Missing descriptor = 1x.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub url: String,
    pub w: Option<f32>,
    pub x: Option<f32>,
}

/// Split `srcset` into candidates. A `data:` URL keeps its commas (the
/// payload is full of them); any other URL ends at a comma.
pub fn parse_srcset(s: &str) -> Vec<Candidate> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let start = i;
        let is_data = s[start..]
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("data:"));
        while i < b.len() && !b[i].is_ascii_whitespace() {
            if b[i] == b',' && !is_data {
                break;
            }
            i += 1;
        }
        let mut url = &s[start..i];
        // a descriptor-less candidate may end at a comma right after the URL
        let trimmed = url.trim_end_matches(',');
        let comma_ended = trimmed.len() != url.len();
        url = trimmed;
        if url.is_empty() {
            if i < b.len() && b[i] == b',' {
                i += 1;
            }
            continue;
        }
        let mut cand = Candidate {
            url: url.to_string(),
            w: None,
            x: None,
        };
        if comma_ended {
            if i < b.len() && b[i] == b',' {
                i += 1;
            }
        } else {
            // descriptors run to the next comma
            let dstart = i;
            while i < b.len() && b[i] != b',' {
                i += 1;
            }
            for tok in s[dstart..i].split_whitespace() {
                if let Some(n) = tok.strip_suffix('w') {
                    if let Ok(v) = n.parse::<f32>() {
                        cand.w = Some(v);
                    }
                } else if let Some(n) = tok.strip_suffix('x') {
                    if let Ok(v) = n.parse::<f32>() {
                        cand.x = Some(v);
                    }
                }
            }
            if i < b.len() {
                i += 1; // eat ','
            }
        }
        out.push(cand);
    }
    out
}

/// The width the `w` descriptors are compared against: the first length in
/// `sizes` that is a bare `<n>px` or `<n>vw`, else the viewport width.
/// shortcut: media conditions inside `sizes` are not evaluated.
fn slot_width(sizes: Option<&str>, vw: f32) -> f32 {
    let Some(sizes) = sizes else { return vw };
    for entry in sizes.split(',') {
        let Some(tok) = entry.split_whitespace().last() else {
            continue;
        };
        if let Some(n) = tok.strip_suffix("px") {
            if let Ok(v) = n.trim().parse::<f32>() {
                return v;
            }
        } else if let Some(n) = tok.strip_suffix("vw") {
            if let Ok(v) = n.trim().parse::<f32>() {
                return vw * v / 100.0;
            }
        }
    }
    vw
}

/// Pick the best `srcset` candidate for a slot width. `None` = no usable
/// candidate (empty/invalid `srcset`).
pub fn pick_srcset(srcset: &str, slot_w: f32) -> Option<String> {
    let cands = parse_srcset(srcset);
    if cands.is_empty() {
        return None;
    }
    // `w` descriptors win when present (the spec forbids mixing them with
    // `x`; a mixed list is simply treated as a `w` list)
    let ws: Vec<&Candidate> = cands.iter().filter(|c| c.w.is_some()).collect();
    if !ws.is_empty() {
        let mut best: Option<&Candidate> = None;
        for c in &ws {
            let w = c.w.unwrap_or(0.0);
            if w >= slot_w && best.is_none_or(|b| w < b.w.unwrap_or(0.0)) {
                best = Some(c);
            }
        }
        // nothing wide enough -> the largest candidate
        let pick = best.or_else(|| {
            ws.iter()
                .copied()
                .reduce(|a, b| if a.w.unwrap_or(0.0) >= b.w.unwrap_or(0.0) { a } else { b })
        });
        return pick.map(|c| c.url.clone());
    }
    // density list: exactly 1x, else the density closest to 1x
    let dens = |c: &Candidate| c.x.unwrap_or(1.0);
    cands
        .iter()
        .find(|c| (dens(c) - 1.0).abs() < 1e-6)
        .or_else(|| {
            cands
                .iter()
                .min_by(|a, b| (dens(a) - 1.0).abs().total_cmp(&(dens(b) - 1.0).abs()))
        })
        .map(|c| c.url.clone())
}

/// Whether a media query list applies at `vw`. Only width features are
/// modeled (`min-width`/`max-width`/`width`); an unknown feature makes its
/// query fail. `screen`/`all`/`only`/`and` are the only outer keywords we
/// accept (`print`/`speech`/`not` fail).
pub fn media_matches(q: &str, vw: f32) -> bool {
    q.split(',').any(|query| media_one(query, vw))
}

fn media_one(query: &str, vw: f32) -> bool {
    let mut ok = true;
    let mut rest = query;
    let mut outside = String::new();
    while let Some(open) = rest.find('(') {
        outside.push_str(&rest[..open]);
        let Some(close) = rest[open..].find(')').map(|c| open + c) else {
            return false; // unbalanced: treat as non-matching
        };
        ok &= width_feature(&rest[open + 1..close], vw);
        rest = &rest[close + 1..];
    }
    outside.push_str(rest);
    for tok in outside.split_whitespace() {
        if !matches!(tok, "and" | "only" | "screen" | "all") {
            return false;
        }
    }
    ok
}

fn width_feature(f: &str, vw: f32) -> bool {
    let Some((name, val)) = f.split_once(':') else {
        return false;
    };
    let Some(px) = length_px(val.trim(), vw) else {
        return false;
    };
    match name.trim().to_ascii_lowercase().as_str() {
        "min-width" => vw >= px,
        "max-width" => vw <= px,
        "width" => (vw - px).abs() < 0.5,
        _ => false,
    }
}

/// `12px` / `12em` / `12rem` / `12vw` -> CSS px. Anything else: None.
fn length_px(v: &str, vw: f32) -> Option<f32> {
    for (suffix, scale) in [("px", 1.0), ("rem", 16.0), ("em", 16.0)] {
        if let Some(n) = v.strip_suffix(suffix) {
            return n.trim().parse::<f32>().ok().map(|n| n * scale);
        }
    }
    v.strip_suffix("vw")
        .and_then(|n| n.trim().parse::<f32>().ok())
        .map(|n| vw * n / 100.0)
}

/// Can we decode this `type`? An absent/empty type is acceptable (the bytes
/// get sniffed); a listed one must be a format `image::decode`/`svg::render`
/// handle.
pub fn type_decodable(t: &str) -> bool {
    let m = t.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(
        m.as_str(),
        "" | "image/webp" | "image/jpeg" | "image/jpg" | "image/png" | "image/svg+xml"
    )
}

/// Chosen URL for this `<img>` at viewport width `vw`: a matching
/// `<picture><source>` first, then the element's own `srcset`, then its
/// plain `src` (returned verbatim, so pages without responsive markup keep
/// the exact URL they always had).
pub fn pick_img_source(dom: &Dom, img: u32, vw: f32) -> Option<String> {
    let parent = dom.nodes[img as usize].parent;
    if parent != 0 && dom.tag(parent) == "picture" {
        for &c in &dom.nodes[parent as usize].children {
            if dom.tag(c) != "source" {
                continue;
            }
            if let Some(t) = dom.attr(c, "type") {
                if !type_decodable(t) {
                    continue; // format we cannot decode: next source
                }
            }
            if let Some(m) = dom.attr(c, "media") {
                if !media_matches(m, vw) {
                    continue;
                }
            }
            let Some(srcset) = dom.attr(c, "srcset") else {
                continue;
            };
            let slot = slot_width(dom.attr(c, "sizes"), vw);
            if let Some(u) = pick_srcset(srcset, slot) {
                return Some(u);
            }
        }
    }
    if let Some(srcset) = dom.attr(img, "srcset").filter(|s| !s.trim().is_empty()) {
        let slot = slot_width(dom.attr(img, "sizes"), vw);
        if let Some(u) = pick_srcset(srcset, slot) {
            return Some(u);
        }
    }
    dom.attr(img, "src")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(markup: &str) -> Dom {
        crate::html::parse(markup)
    }

    /// The first `<img>` node in document order.
    fn img_node(dom: &Dom) -> u32 {
        (0..dom.nodes.len() as u32)
            .find(|&i| dom.tag(i) == "img")
            .expect("test markup has an <img>")
    }

    fn chosen(markup: &str, vw: f32) -> Option<String> {
        let dom = parse(markup);
        pick_img_source(&dom, img_node(&dom), vw)
    }

    #[test]
    fn plain_img_src_is_returned_verbatim() {
        // regression guard: no responsive markup -> the exact `src` string,
        // so probe/decode/cache (and therefore paint) are unchanged
        let src = "/assets/hero.png?w=100&h=50#frag";
        assert_eq!(
            chosen(&format!(r#"<body><img src="{src}"></body>"#), 800.0).as_deref(),
            Some(src)
        );
        // no src and no srcset -> no slot (same as before)
        assert_eq!(chosen("<body><img alt=x></body>", 800.0), None);
    }

    #[test]
    fn w_descriptors_track_the_viewport() {
        let markup = r#"<body><img src="fallback.png"
            srcset="small.png 300w, mid.png 600w, big.png 1200w"></body>"#;
        assert_eq!(chosen(markup, 800.0).as_deref(), Some("big.png"));
        assert_eq!(chosen(markup, 500.0).as_deref(), Some("mid.png"));
        assert_eq!(chosen(markup, 300.0).as_deref(), Some("small.png"));
        // nothing wide enough -> largest
        assert_eq!(chosen(markup, 2000.0).as_deref(), Some("big.png"));
        // unsorted list still yields the smallest sufficient candidate
        let unsorted = r#"<body><img srcset="big.png 1200w, small.png 300w, mid.png 600w"></body>"#;
        assert_eq!(chosen(unsorted, 500.0).as_deref(), Some("mid.png"));
    }

    #[test]
    fn sizes_first_length_sets_the_slot() {
        let markup = r#"<body><img sizes="400px"
            srcset="a.png 300w, b.png 600w, c.png 1200w"></body>"#;
        // 400px slot: the 600w candidate is the smallest sufficient one
        assert_eq!(chosen(markup, 1000.0).as_deref(), Some("b.png"));
        // vw lengths resolve against the viewport
        let vw = r#"<body><img sizes="50vw"
            srcset="a.png 300w, b.png 600w, c.png 1200w"></body>"#;
        assert_eq!(chosen(vw, 1000.0).as_deref(), Some("b.png"));
    }

    #[test]
    fn x_descriptors_pick_the_1x_entry() {
        let markup = r#"<body><img srcset="one.png 1x, two.png 2x, three.png 3x"></body>"#;
        assert_eq!(chosen(markup, 800.0).as_deref(), Some("one.png"));
        let no_one = r#"<body><img srcset="two.png 2x, three.png 3x"></body>"#;
        assert_eq!(chosen(no_one, 800.0).as_deref(), Some("two.png"));
        let bare = r#"<body><img srcset="only.png"></body>"#;
        assert_eq!(chosen(bare, 800.0).as_deref(), Some("only.png"));
    }

    #[test]
    fn srcset_parses_data_urls_and_tight_commas() {
        let cands = parse_srcset("data:image/png;base64,AA,BB 2x, plain.png 3x");
        assert_eq!(cands.len(), 2, "{cands:?}");
        assert_eq!(cands[0].url, "data:image/png;base64,AA,BB");
        assert_eq!(cands[0].x, Some(2.0));
        assert_eq!(cands[1].url, "plain.png");
        // no space after the separator comma
        let tight = parse_srcset("a.png 1x,b.png 2x");
        assert_eq!(
            tight.iter().map(|c| c.url.as_str()).collect::<Vec<_>>(),
            ["a.png", "b.png"]
        );
        assert_eq!(parse_srcset("a.png,b.png").len(), 2);
    }

    #[test]
    fn picture_prefers_a_decodable_matching_source() {
        let webp = r#"<body><picture>
            <source type="image/webp" srcset="hero.webp 1x">
            <img src="hero.jpg" srcset="hero-2x.jpg 2x">
        </picture></body>"#;
        assert_eq!(chosen(webp, 800.0).as_deref(), Some("hero.webp"));
        // a media query that does not match falls through to the <img>
        let narrow = r#"<body><picture>
            <source media="(min-width: 900px)" srcset="wide.webp">
            <img src="hero.jpg">
        </picture></body>"#;
        assert_eq!(chosen(narrow, 800.0).as_deref(), Some("hero.jpg"));
        assert_eq!(chosen(narrow, 1000.0).as_deref(), Some("wide.webp"));
        // a type we cannot decode falls through too
        let avif = r#"<body><picture>
            <source type="image/avif" srcset="hero.avif">
            <img src="hero.png">
        </picture></body>"#;
        assert_eq!(chosen(avif, 800.0).as_deref(), Some("hero.png"));
        // max-width query
        let small = r#"<body><picture>
            <source media="(max-width: 600px)" srcset="small.webp">
            <img src="hero.jpg">
        </picture></body>"#;
        assert_eq!(chosen(small, 500.0).as_deref(), Some("small.webp"));
        assert_eq!(chosen(small, 800.0).as_deref(), Some("hero.jpg"));
    }

    #[test]
    fn picture_without_matching_source_uses_img_srcset() {
        let markup = r#"<body><picture>
            <source type="image/webp" srcset="p.webp">
            <img src="fallback.jpg" srcset="s.jpg 400w, l.jpg 1000w">
        </picture></body>"#;
        // the webp source wins first; with only an unsupported type it falls
        // back to the <img>'s own responsive set
        let markup2 = markup.replace("image/webp", "image/avif");
        assert_eq!(chosen(markup, 800.0).as_deref(), Some("p.webp"));
        assert_eq!(chosen(&markup2, 800.0).as_deref(), Some("l.jpg"));
    }

    #[test]
    fn media_query_features() {
        assert!(media_matches("(min-width: 800px)", 800.0));
        assert!(!media_matches("(min-width: 801px)", 800.0));
        assert!(media_matches("screen and (max-width: 1000px)", 800.0));
        assert!(media_matches("all", 800.0));
        assert!(!media_matches("print", 800.0));
        assert!(!media_matches("(orientation: landscape)", 800.0));
        assert!(media_matches("(min-width: 400px), print", 800.0));
        assert!(!media_matches("(min-width: 900px), (max-width: 500px)", 800.0));
        assert!(media_matches("(min-width: 50em)", 900.0));
    }

    #[test]
    fn type_decodable_set() {
        for t in [
            "",
            "image/webp",
            "IMAGE/WEBP",
            "image/jpeg",
            "image/jpg; charset=binary",
            "image/png",
            "image/svg+xml",
        ] {
            assert!(type_decodable(t), "{t:?}");
        }
        for t in ["image/avif", "image/gif", "video/mp4", "image/jxl"] {
            assert!(!type_decodable(t), "{t:?}");
        }
    }
}
