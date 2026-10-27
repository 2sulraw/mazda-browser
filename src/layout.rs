//! DOM → styled atoms → line layout using real glyph metrics.
//! shortcut: only inline/block split + word wrap (no float/flex/absolute);
//! upgrade when a real CSS engine lands (Phase 1b+).

use crate::css::Sheet;
use crate::html::{Dom, NodeKind};

#[derive(Clone, Copy, PartialEq)]
pub struct Style {
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: u32,
    pub underline: bool,
    pub mono: bool,
}

impl Style {
    pub const fn new() -> Self {
        Style {
            size: 15.0,
            bold: false,
            italic: false,
            color: 0x00_e8_e8_e8,
            underline: false,
            mono: false,
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Atom {
    pub text: String,
    pub style: Style,
    /// forced line break (block element boundary or <br>)
    pub hard_break: bool,
    /// raw href of the enclosing <a>, resolved against base URL only on click
    pub link: Option<std::rc::Rc<str>>,
}

/// Apply presentational defaults for an element tag.
fn style_for(tag: &str, base: Style) -> Style {
    let mut s = base;
    match tag {
        "h1" => {
            s.size = (base.size * 2.0).max(28.0);
            s.bold = true;
        }
        "h2" => {
            s.size = (base.size * 1.6).max(24.0);
            s.bold = true;
        }
        "h3" => {
            s.size = (base.size * 1.35).max(20.0);
            s.bold = true;
        }
        "h4" | "h5" | "h6" => {
            s.size = (base.size * 1.15).max(17.0);
            s.bold = true;
        }
        "b" | "strong" | "th" => s.bold = true,
        "i" | "em" | "cite" | "var" => s.italic = true,
        "u" | "ins" => s.underline = true,
        "code" | "kbd" | "samp" | "pre" | "tt" => {
            s.mono = true;
            s.size = base.size - 1.0;
        }
        "a" => {
            s.color = 0x00_6a_b3_ff;
            s.underline = true;
        }
        "small" => s.size = base.size - 2.0,
        "del" => s.color = 0x00_cc_77_77,
        _ => {}
    }
    s
}

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div" | "section" | "article" | "header" | "footer" | "nav" | "aside"
            | "main" | "blockquote" | "figure" | "figcaption" | "ul" | "ol" | "li" | "dl"
            | "dt" | "dd" | "table" | "tr" | "td" | "th" | "caption" | "form" | "fieldset"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "hr" | "address" | "center"
    )
}

/// Walk the subtree under `node`, emitting styled atoms in document order.
/// `sheet` is the precompiled CSS rule list (Phase 1b); `style` should already
/// be the cascade result for `node` itself (main resolves body before calling).
pub fn walk(dom: &Dom, node: u32, style: Style, sheet: &Sheet, out: &mut Vec<Atom>) {
    walk_link(dom, node, style, sheet, None, out);
}

/// Elements whose content is never rendered (also covers `<style>`/`<script>`
/// that appear inside body — head is already excluded by `body_root`).
const NON_RENDERED: &[&str] = &[
    "script", "style", "noscript", "template", "head", "title", "meta", "link", "svg",
];

fn walk_link(
    dom: &Dom,
    node: u32,
    style: Style,
    sheet: &Sheet,
    link: Option<std::rc::Rc<str>>,
    out: &mut Vec<Atom>,
) {
    for &child in &dom.nodes[node as usize].children {
        match &dom.nodes[child as usize].kind {
            NodeKind::Text(t) => {
                let t = t.trim_matches('\u{a0}');
                if !t.is_empty() {
                    out.push(Atom {
                        text: t.to_string(),
                        style,
                        hard_break: false,
                        link: link.clone(),
                    });
                }
            }
            NodeKind::Element(..) => {
                let tag = dom.tag(child);
                if NON_RENDERED.contains(&tag) {
                    continue;
                }
                let block = is_block(tag);
                if tag == "br" || tag == "hr" {
                    out.push(Atom {
                        text: String::new(),
                        style,
                        hard_break: true,
                        link: None,
                    });
                    continue;
                }
                let child_style = if tag == "body" { style } else { style_for(tag, style) };
                // author CSS (rules + inline style) cascades over UA defaults
                let outcome = sheet.resolve(dom, child, child_style, dom.attr(child, "style"));
                if outcome.display_none {
                    continue;
                }
                let child_style = outcome.style;
                // <a href> opens a link scope; nested <a> is invalid HTML so inherit-or-replace is enough
                let child_link = if tag == "a" {
                    dom.attr(child, "href")
                        .map(|h| std::rc::Rc::from(h))
                        .or_else(|| link.clone())
                } else {
                    link.clone()
                };
                let mark = out.len();
                walk_link(dom, child, child_style, sheet, child_link, out);
                if block && out.len() > mark {
                    // ensure a block ends on its own line
                    if !out.last().unwrap().hard_break {
                        out.push(Atom {
                            text: String::new(),
                            style: child_style,
                            hard_break: true,
                            link: None,
                        });
                    }
                    // and starts on a fresh one
                    out.insert(
                        mark,
                        Atom {
                            text: String::new(),
                            style,
                            hard_break: true,
                            link: None,
                        },
                    );
                }
            }
        }
    }
}

pub struct PositionedRun {
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub style: Style,
    pub width: f32,
    pub link: Option<std::rc::Rc<str>>,
}

pub struct Layout {
    pub runs: Vec<PositionedRun>,
    pub height: f32,
    /// x, y, w, thickness, color (matches the run's text color)
    pub underline_rects: Vec<(f32, f32, f32, f32, u32)>,
}

/// Lay atoms into lines. `measure(text, style)` returns pixel width.
pub fn layout(atoms: &[Atom], max_width: f32, measure: &dyn Fn(&str, &Style) -> f32) -> Layout {
    let mut runs = Vec::new();
    let mut underline_rects = Vec::new();
    let mut y = 0.0f32;
    let mut x = 0.0f32;
    let mut line_height = 0.0f32;

    // split text into word atoms first so wrapping is per-word
    let mut words: Vec<Atom> = Vec::new();
    for a in atoms {
        if a.hard_break {
            words.push(Atom {
                text: String::new(),
                style: a.style,
                hard_break: true,
                link: None,
            });
            continue;
        }
        // split on any whitespace, remembering which had a trailing space
        let mut first = true;
        for w in a.text.split(|c: char| c.is_whitespace()) {
            if w.is_empty() {
                continue;
            }
            if !first {
                words.push(Atom {
                    text: " ".into(),
                    style: a.style,
                    hard_break: false,
                    link: a.link.clone(),
                });
            }
            first = false;
            words.push(Atom {
                text: w.to_string(),
                style: a.style,
                hard_break: false,
                link: a.link.clone(),
            });
        }
    }

    let space_w = |s: &Style| measure(" ", s);

    for w in &words {
        if w.hard_break {
            if x > 0.0 {
                y += line_height;
                x = 0.0;
                line_height = 0.0;
                y += w.style.size * 0.6; // paragraph gap only after content, not between empty breaks
            }
            continue;
        }
        if w.text == " " {
            if x > 0.0 {
                x += space_w(&w.style);
            }
            continue;
        }
        let ww = measure(&w.text, &w.style);
        if x > 0.0 && x + ww > max_width {
            y += line_height;
            x = 0.0;
            line_height = 0.0;
        }
        let lh = w.style.size * 1.35;
        runs.push(PositionedRun {
            x,
            y,
            text: w.text.clone(),
            style: w.style,
            width: ww,
            link: w.link.clone(),
        });
        if w.style.underline {
            underline_rects.push((x, y + lh - 2.0, ww, 1.0, w.style.color));
        }
        x += ww;
        line_height = line_height.max(lh);
    }
    y += line_height;

    Layout {
        runs,
        height: y,
        underline_rects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_atom(text: &str, hard: bool) -> Atom {
        Atom {
            text: text.into(),
            style: Style::new(),
            hard_break: hard,
            link: None,
        }
    }

    #[test]
    fn wraps_at_width() {
        let atoms = vec![mk_atom("word word word word", false)];
        // 4px per char + space => 9px words, wraps at 20px
        let l = layout(&atoms, 20.0, &|s, _| s.len() as f32 * 4.0);
        let ys: std::collections::HashSet<i32> =
            l.runs.iter().map(|r| r.y as i32).collect();
        assert!(ys.len() >= 2, "expected multiple lines, got {}", ys.len());
        for r in &l.runs {
            assert!(r.x + r.width <= 20.0 + 0.001, "run overflows: {:?}", r.text);
        }
    }

    #[test]
    fn hard_break_starts_new_line() {
        let atoms = vec![mk_atom("a", false), mk_atom("", true), mk_atom("b", false)];
        let l = layout(&atoms, 100.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l.runs.len(), 2);
        assert!(l.runs[1].y > l.runs[0].y);
    }

    #[test]
    fn tabs_and_newlines_never_reach_draw() {
        // regression: "\n\t" chunks used to become .notdef box glyphs
        let atoms = vec![mk_atom("main\tmenu\n\theader", false)];
        let l = layout(&atoms, 500.0, &|s, _| s.len() as f32 * 4.0);
        let texts: Vec<&str> = l.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["main", "menu", "header"]);
        for r in &l.runs {
            assert!(r.text.chars().all(|c| !c.is_whitespace()));
        }
    }

    #[test]
    fn long_word_overflows_rather_than_loop() {
        let atoms = vec![mk_atom("aaaaaaaaaa", false)]; // wider than max
        let l = layout(&atoms, 10.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l.runs.len(), 1);
    }

    #[test]
    fn link_survives_word_split() {
        let mut a = mk_atom("click here now", false);
        a.link = Some(std::rc::Rc::from("/page"));
        let l = layout(&[a], 500.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l.runs.len(), 3);
        for r in &l.runs {
            assert_eq!(r.link.as_deref(), Some("/page"));
        }
    }

    #[test]
    fn non_link_runs_have_no_href() {
        let l = layout(&[mk_atom("plain text", false)], 500.0, &|s, _| s.len() as f32 * 4.0);
        assert!(l.runs.iter().all(|r| r.link.is_none()));
    }
}
