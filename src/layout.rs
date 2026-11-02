//! DOM → styled atoms → line layout using real glyph metrics.
//! shortcut: only inline/block split + word wrap, plus a SIMPLIFIED
//! float/clear/vertical-align model (each shortcut documented in
//! `block()`); upgrade when a real CSS engine lands (Phase 1b+).

use crate::css::{
    content_text, decl, marker_text, Align, BgPos, BgRepeat, BgSize, BoxStyle, Clear, Cursor,
    Dir, Display, Float, Justify, Len, ObjFit, Pos, PseudoEl, Sheet, Track, VAlign,
};
use crate::html::{Dom, NodeKind};
use crate::image::ImageData;
use std::collections::HashMap;

/// Resolved generic font family (CSS `font-family` stack → first known
/// generic; unknown names are skipped, default = Sans).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fam {
    Sans,
    Serif,
    Mono,
}

/// `white-space` — inherited. The engine's model is collapse + wrap, so
/// each keyword maps onto those two axes:
/// - `normal`: fold every whitespace run to one separator, wrap (unchanged
///   since forever — byte-identical when no `white-space` is declared)
/// - `nowrap`: fold, but never wrap (a run may exceed the box; it is only
///   clipped if an ancestor's `overflow` says so)
/// - `pre`: keep every space/tab verbatim, `\n` is a hard break
///   ([`BoxNode::hard_break`]), never wrap at spaces
/// - `pre-wrap`: keep whitespace AND wrap
/// - `pre-line`: fold spaces (like normal) but honor `\n` and wrap
///
/// shortcut: `white-space` is per TEXT NODE — no propagation out of a
/// `<pre>` element's inline descendants with mixed normal siblings (the
/// UA default on `<pre>` itself is `Ws::Pre`, see `style_for`, so plain
/// `<pre>` content works; an inline child that redeclares gets its own
/// mode). `break-word`/`overflow-wrap` are out of scope (this batch only
/// breaks where the old breaker already broke: whitespace).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Ws {
    Normal,
    Nowrap,
    Pre,
    PreWrap,
    PreLine,
}

impl Ws {
    /// soft wrapping allowed — only `nowrap` and `pre` never wrap
    pub fn wrap(self) -> bool {
        matches!(self, Ws::Normal | Ws::PreWrap | Ws::PreLine)
    }
    /// whitespace runs keep their exact text (no folding)
    pub fn preserve(self) -> bool {
        matches!(self, Ws::Pre | Ws::PreWrap)
    }
    /// `\n` inside the text is a line break (split into hard breaks)
    pub fn honors_newline(self) -> bool {
        matches!(self, Ws::Pre | Ws::PreWrap | Ws::PreLine)
    }
}

/// `list-style-type` — inherited (the UA sets it on `<ul>`/`<ol>` in
/// [`style_for`], exactly where the other UA defaults live, and `<li>`
/// inherits the parent list's value). `None` = no marker box at all.
/// shortcut: only these ten values — no `disclosure-*`, CJK/`ethiopic-*`
/// styles, `symbols()` or `@counter-style`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ListStyleType {
    None,
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

/// `list-style-position` — inherited like the type; `Outside` (default)
/// hangs the marker left of the content box, `Inside` puts it in the first
/// line's flow.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ListPos {
    Outside,
    Inside,
}

/// `text-transform` — inherited, applied ONCE when a text box is built
/// (see [`transform_text`]): runs, wrapping, measurement and drawing all
/// see the final string, so nothing downstream needs to know about it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tt {
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

#[derive(Clone, Copy, PartialEq)]
pub struct Style {
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub color: u32,
    pub underline: bool,
    pub fam: Fam,
    /// 0 = left, 1 = center, 2 = right (inherited, text-align)
    pub text_align: u8,
    /// line-height in px; 0 = default (size * 1.35) — inherited
    pub lh: f32,
    /// `white-space` — inherited, see [`Ws`]
    pub ws: Ws,
    /// `text-transform` — inherited, see [`Tt`]
    pub text_transform: Tt,
    /// `letter-spacing` in px — inherited; extra advance between each pair
    /// of glyphs. Layout and draw_text both add it via [`spacing_extra`]
    pub letter_spacing: f32,
    /// `word-spacing` in px — inherited; extra advance on every whitespace
    /// char. Same [`spacing_extra`] rule on both sides of the pipeline
    pub word_spacing: f32,
    /// interned id of the full `font-family` comma stack (0 = none → generic
    /// pick); the names live in the shared table Fonts reads at pick time
    pub fam_stack: u16,
    /// `list-style-type` — inherited (see [`ListStyleType`])
    pub list_type: ListStyleType,
    /// `list-style-position` — inherited (see [`ListPos`])
    pub list_pos: ListPos,
}

impl Style {
    pub const fn new() -> Self {
        Style {
            size: 15.0,
            bold: false,
            italic: false,
            color: 0x00_00_00_00, // UA default = black (matches every browser)
            underline: false,
            fam: Fam::Sans,
            text_align: 0,
            lh: 0.0,
            ws: Ws::Normal,
            text_transform: Tt::None,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            fam_stack: 0,
            // CSS initial value is `disc`; `<ol>` overrides it in style_for
            list_type: ListStyleType::Disc,
            list_pos: ListPos::Outside,
        }
    }

    /// resolved line box height for this style
    pub fn line_height(&self) -> f32 {
        if self.lh > 0.0 {
            self.lh
        } else {
            self.size * 1.35
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self::new()
    }
}

/// The tracking delta layout adds on top of raw glyph advances for one
/// string: letter-spacing after EVERY glyph (count × spacing — the spec
/// adds it after the last glyph too, which is why a tracked run is
/// `spacing × count` wider than the plain one) plus word-spacing on every
/// whitespace char. This is THE shared rule — `layout_from`'s line builder,
/// `natural_w` and the form-caret measure all add it to `measure(...)`
/// results, and `draw_text` (main.rs) replays the exact same per-glyph
/// arithmetic so painted ink and the measured `PositionedRun::width` never
/// diverge (both sides comment the coupling).
///
/// shortcut: no script-specific spacing rules (CJK inter-character
/// spacing, justification interplay); the trailing letter-space past the
/// last glyph counts toward the run width even though it paints no ink
/// (line ends thus sit one spacing wider — matches how the draw side
/// advances its cursor).
pub fn spacing_extra(text: &str, style: &Style) -> f32 {
    let n = text.chars().count();
    let ls = style.letter_spacing * n as f32;
    let ws = text.chars().filter(|c| c.is_whitespace()).count() as f32 * style.word_spacing;
    ls + ws
}

/// `text-transform` applied ONCE when a text box is built (build_into's
/// text branch and pseudo generated content), so runs, wrapping,
/// measurement and drawing all see the final string with zero render
/// changes.
///
/// shortcut: ASCII-only case mapping — non-ASCII letters pass through
/// unchanged (no locale forms such as Turkish dotless i, no ß→SS
/// expansion, no `full-width`/`wide` forms); `capitalize` uppercases the
/// first ASCII letter of each whitespace-separated word WITHIN this text
/// node (a word split across inline elements transforms per fragment) and
/// the rest of the word keeps its authored case.
pub fn transform_text(t: &str, tt: Tt) -> std::borrow::Cow<'_, str> {
    match tt {
        Tt::None => std::borrow::Cow::Borrowed(t),
        Tt::Uppercase => std::borrow::Cow::Owned(t.to_ascii_uppercase()),
        Tt::Lowercase => std::borrow::Cow::Owned(t.to_ascii_lowercase()),
        Tt::Capitalize => {
            let mut out = String::with_capacity(t.len());
            // still at a word start until the first ASCII LETTER lands —
            // digits/symbols before it don't consume the titlecasing
            let mut word = true;
            for c in t.chars() {
                if c.is_ascii_whitespace() {
                    word = true;
                    out.push(c);
                } else if word {
                    if c.is_ascii_alphabetic() {
                        word = false;
                        out.push(c.to_ascii_uppercase());
                    } else {
                        out.push(c);
                    }
                } else {
                    out.push(c);
                }
            }
            std::borrow::Cow::Owned(out)
        }
    }
}

pub struct Atom {
    pub text: String,
    pub style: Style,
    /// forced line break (block element boundary or <br>)
    pub hard_break: bool,
    /// raw href of the enclosing <a>, resolved against base URL only on click
    pub link: Option<std::rc::Rc<str>>,
    /// DOM element the text belongs to (hover hit-testing target); None for
    /// synthetic text (form-state value leaves, test atoms)
    pub node: Option<u32>,
    /// the text's effective `cursor` (inherited down the box tree at build
    /// time, see `build_into`); None = no declaration anywhere above
    pub cursor: Option<Cursor>,
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
        "code" | "kbd" | "samp" | "tt" => {
            s.fam = Fam::Mono;
            s.size = base.size - 1.0;
        }
        // <pre>: same mono UA default plus white-space: pre — the parser
        // keeps the raw text (html.rs never folds whitespace), so hooking
        // the Style here is what makes <pre> blocks actually preserve their
        // spaces and line breaks; author CSS cascades over this base and
        // can still override (shortcut: the newline right after the tag
        // isn't stripped — spec does — but a LEADING hard break is a no-op
        // in layout, so it renders like the stripped version)
        "pre" => {
            s.fam = Fam::Mono;
            s.size = base.size - 1.0;
            s.ws = Ws::Pre;
        }
        "a" => {
            s.color = 0x00_6a_b3_ff;
            s.underline = true;
        }
        "small" => s.size = base.size - 2.0,
        "del" => s.color = 0x00_cc_77_77,
        // list UA defaults: the list ELEMENT owns the type and `li` inherits
        // it, exactly the UA-sheet shape (`ul { list-style-type: disc }`,
        // `ol { list-style-type: decimal }`). A nested list therefore resets
        // its own type instead of inheriting the outer one.
        // This engine has no textual UA sheet — `style_for` + `is_block` ARE
        // its UA defaults (headings, links, `<pre>` all live here), so the
        // list defaults land here too. `list-style-position` keeps its
        // inherited value (initial `outside`).
        "ul" => s.list_type = ListStyleType::Disc,
        "ol" => s.list_type = ListStyleType::Decimal,
        _ => {}
    }
    s
}

/// Box-chrome defaults for form controls (`<select>`/`<input>`/
/// `<textarea>`): the bordered, filled, padded box a desktop browser
/// draws around them. Applied AFTER the author cascade, but only to
/// properties the author did NOT declare — css.rs sets a [`decl`] bit on
/// every box property an author/inline rule touches, so a declared value
/// (including a reset to initial: `border:none`, `padding:0`,
/// `background:none`) always wins over the UA chrome, and a color-less
/// `border:1px solid` renders currentcolor instead of the UA gray.
fn control_chrome(bx: &mut BoxStyle, min_h: f32, default_w: Option<f32>) {
    if bx.declared & decl::BORDER == 0 {
        bx.border = [Len::Px(1.0); 4];
    }
    if bx.declared & decl::BCOLOR == 0 {
        bx.bcolor = Some(0x00_76_76_76); // UA control-border gray
    }
    if bx.declared & decl::BG == 0 {
        bx.bg = Some(0x00_f4_f4_f4); // light control fill
    }
    if bx.declared & decl::PAD == 0 {
        bx.pad = [Len::Px(0.0), Len::Px(3.0), Len::Px(0.0), Len::Px(3.0)];
    }
    if bx.declared & decl::RADIUS == 0 {
        bx.radius = Len::Px(2.0);
    }
    if bx.declared & decl::MIN_HEIGHT == 0 {
        bx.min_height = Some(Len::Px(min_h));
    }
    if bx.declared & decl::WIDTH == 0 {
        bx.width = default_w.map(Len::Px);
    }
}

/// Text contained by `node`, NOT descending into nested `<option>`s: the
/// soup parser nests an unclosed `<option>` inside the previous one, and
/// each option's label belongs to it alone. (Textareas are raw text, so
/// their default value comes out whole.)
///
/// `svg`/`script`/`style` subtrees are skipped: their payload is markup or
/// commands (svg holds its shapes as a RAW_TEXT child), never user-visible
/// label text — an icon `<svg>` inside a `<button>` must not leak
/// `<path d="…"/>` source into the button's label. The svg image pipeline
/// reads svg's own text children directly, never through this fn.
fn contained_text(dom: &Dom, node: u32) -> String {
    fn is_hidden_payload(tag: &str) -> bool {
        matches!(tag, "svg" | "script" | "style")
    }
    let mut s = String::new();
    for &c in &dom.nodes[node as usize].children {
        match &dom.nodes[c as usize].kind {
            NodeKind::Text(t) => s.push_str(t),
            NodeKind::Element(..) => {
                let tag = dom.tag(c);
                if tag != "option" && !is_hidden_payload(&tag) {
                    s.push_str(&contained_text(dom, c));
                }
            }
        }
    }
    s.trim().to_string()
}

/// Labels of every `<option>` under `node` in document order, as
/// (text, selected). Recurses through `<optgroup>` and into soup-nested
/// options so `<option>a<option>b` still yields two entries.
fn option_labels(dom: &Dom, node: u32, out: &mut Vec<(String, bool)>) {
    for &c in &dom.nodes[node as usize].children {
        if matches!(&dom.nodes[c as usize].kind, NodeKind::Element(..)) {
            if dom.tag(c) == "option" {
                out.push((contained_text(dom, c), dom.attr(c, "selected").is_some()));
            }
            option_labels(dom, c, out);
        }
    }
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

/// Replaced-element payload for `<img>`: src before fetch, intrinsic size
/// after the header probe, pixels only after layout (downscaled to display
/// size — RAM rule). Failed/unsupported stay dim-only → box reserves space,
/// nothing draws.
pub enum ImgSlot {
    Pending(std::rc::Rc<str>),
    /// fetched, header parsed: intrinsic size known, pixels not decoded
    Probed { src: std::rc::Rc<str>, w: u32, h: u32 },
    Ready(std::rc::Rc<ImageData>),
}

impl ImgSlot {
    /// intrinsic (w, h) if known
    pub fn dims(&self) -> Option<(u32, u32)> {
        match self {
            ImgSlot::Pending(_) => None,
            ImgSlot::Probed { w, h, .. } => Some((*w, *h)),
            ImgSlot::Ready(d) => Some((d.w, d.h)),
        }
    }
}

/// Resolve `object-fit`/`object-position` into (content rect, crop window)
/// inside `bx` (the replaced element's padding box). The content rect keeps
/// the intrinsic aspect ratio for every mode but `fill`; `object-position`
/// anchors it with the same `(box − content) × %` formula background-position
/// uses, so `cover` crops symmetrically at the default 50%/50%. The crop
/// window is the intersection with the box — Some only when the content
/// overflows, so decode knows to trim it.
/// shortcut: subpixel filtering is out of scope (nearest-neighbour resample);
/// the fit area is the padding box (same as `background-size`), so a padded
/// replaced box letterboxes inside its border, not strictly its content box.
fn fit_content_rect(
    fit: ObjFit,
    pos: [BgPos; 2],
    bx: (f32, f32, f32, f32),
    iw: f32,
    ih: f32,
) -> ((f32, f32, f32, f32), Option<(f32, f32, f32, f32)>) {
    let (bx, by, bw, bh) = bx;
    if fit == ObjFit::Fill {
        return ((bx, by, bw, bh), None);
    }
    let iw = iw.max(1.0);
    let ih = ih.max(1.0);
    let s = match fit {
        ObjFit::Fill => unreachable!(),
        ObjFit::Contain => (bw / iw).min(bh / ih),
        ObjFit::Cover => (bw / iw).max(bh / ih),
        ObjFit::None => 1.0,
        ObjFit::ScaleDown => (bw / iw).min(bh / ih).min(1.0),
    };
    let (cw, ch) = ((iw * s).max(0.0), (ih * s).max(0.0));
    let ox = match pos[0] {
        BgPos::Pct(p) => (bw - cw) * p / 100.0,
        BgPos::Px(v) => v,
    };
    let oy = match pos[1] {
        BgPos::Pct(p) => (bh - ch) * p / 100.0,
        BgPos::Px(v) => v,
    };
    let rect = (bx + ox, by + oy, cw, ch);
    // crop window = content ∩ box; None when the content fits (nothing to
    // trim), which keeps `contain`/`scale-down` on the plain draw path
    let (x0, y0) = (rect.0.max(bx), rect.1.max(by));
    let (x1, y1) = ((rect.0 + cw).min(bx + bw), (rect.1 + ch).min(by + bh));
    let clipped = x0 > rect.0 || y0 > rect.1 || x1 < rect.0 + cw || y1 < rect.1 + ch;
    (rect, clipped.then_some((x0, y0, x1, y1)))
}

/// What a form-control box does — decides hit-test semantics, focus
/// painting and the value shown. Built from the element tag + `type`
/// attribute during [`build`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FormKind {
    /// text-ish `<input>` (text/search/email/url/tel/number/absent type)
    /// and `<textarea>` — focusable, takes typed text, shows a caret
    TextInput { password: bool },
    /// `<input type=checkbox>` — small square, toggle on click
    Checkbox,
    /// `<input type=radio>` — small circle, select one in a group
    Radio,
    /// `<input type=submit|button|reset>` and the `<button>` element —
    /// activation on click (shortcut: no form POST, the shell logs
    /// `[form] submit name=value`)
    Submit,
    /// `<select>` — closed one-line display only (no popup yet; kept
    /// working as before, but now hit-testable for a future popup)
    Select,
    /// other `<input>` types (range/color/date/file/…) — chrome box
    /// shows the value attr, but no widget art and no interaction yet
    Generic,
}

impl FormKind {
    /// does a click "activate" this control (as opposed to focusing a
    /// text field or opening a future popup)?
    pub fn activates(&self) -> bool {
        matches!(self, FormKind::Submit)
    }
    /// does this control accept typed text (focus + caret)?
    pub fn takes_text(&self) -> bool {
        matches!(self, FormKind::TextInput { .. })
    }
}

/// Form-control identity carried from build into the laid output: which
/// DOM node this box came from, what it does, and its initial raw value
/// (seeded into [`Layout::form_values`]; password masking is display-
/// only, storage always keeps the raw text).
pub struct FormCtl {
    /// DOM node index in the [`Dom`] — stable across re-layout, the key
    /// the focus slot and value map are saved under
    pub node: u32,
    pub kind: FormKind,
    /// initial raw value: `value` attribute / textarea content / the
    /// selected option label / the button label (never masked)
    pub value: String,
}

/// One laid-out form control: its border-box rect in document coords
/// (scroll offset NOT applied) plus interaction metadata. Collected in
/// tree order by [`layout_in`].
#[derive(Clone, Copy, Debug)]
pub struct FormBox {
    /// DOM node index (see [`FormCtl::node`])
    pub node: u32,
    /// border-box rect: x, y, w, h in document coordinates
    pub rect: (f32, f32, f32, f32),
    pub kind: FormKind,
    pub focused: bool,
    /// inside a `position: fixed` subtree — `y` is viewport-relative
    pub fixed: bool,
}

/// Focus + value state the shell carries across re-layouts. Both maps are
/// keyed by DOM node id (NOT by [`FormBox`] index), so resizing the
/// window or re-running layout keeps the same field focused and the same
/// values shown. The SHELL owns the editable string — it edits on
/// keystrokes via [`Layout::set_form_value`] and re-runs
/// [`layout_in_forms`]; the engine only stores and displays.
#[derive(Clone, Default)]
pub struct FormState {
    /// focused control's DOM node id (None = nothing focused)
    pub focused: Option<u32>,
    /// live values by DOM node id (raw text; password masking is display-
    /// only)
    pub values: HashMap<u32, String>,
}

/// Box tree node: a text run (leaf) or an element box (block/flex/grid).
pub struct BoxNode {
    pub is_text: bool,
    /// synthetic background-image placeholder (first child of the element
    /// its `background-image` targets): carries an [`ImgSlot`] so the
    /// shared `<img>` pipeline fetches/decodes it; `place()` sizes and
    /// positions it per the element's `background-size`/`background-
    /// position` inside the padding box, reserves no flow space
    pub is_bg: bool,
    /// synthetic mask-image placeholder (child of the element its
    /// `mask-image` targets): same pipeline and geometry mechanics as
    /// [`is_bg`], but flagged so a live one REPLACES its box's own
    /// background at paint (`collect_paint` drops the bg-color rect and
    /// the background image, the mask draws tinted with the bg color —
    /// see `BoxStyle::mask_image` for the shortcuts)
    pub is_mask: bool,
    /// synthesized list-item marker run (`<li>` with `list-style-type` ≠
    /// none). `out` position markers are painted in the list's left gutter
    /// by `block()` without advancing the line; `inside` ones are ordinary
    /// in-flow text leaves (leading the first line)
    pub is_marker: bool,
    /// tiling region for a REPEATING background placeholder: the parent's
    /// padding box (x0, y0, x1, y1) in document coords, recorded by
    /// `place()`; `collect_paint` emits one entry per tile over it. None =
    /// draw ONE entry at (x, y, w, h) — no-repeat or undeclared repeat
    pub bg_tile_region: Option<(f32, f32, f32, f32)>,
    pub style: Style,
    pub bx: BoxStyle,
    pub text: String,
    /// forced line break (<br>/<hr>)
    pub hard_break: bool,
    /// raw href of the enclosing <a>, resolved against base URL only on click
    pub link: Option<std::rc::Rc<str>>,
    /// DOM element node that produced this box — hover hit-testing target
    /// (`None` for synthetic boxes: background placeholders, form chrome
    /// leaves, test-built trees). Text leaves carry their CONTAINING
    /// element (text-node ids never become the hover cell).
    pub node: Option<u32>,
    /// `<img>` replacement data (None for every other box)
    pub img: Option<ImgSlot>,
    /// object-fit content rect in document coords — where the image's pixels
    /// actually go. None = the box itself (initial `fill` / no declaration),
    /// which keeps the legacy draw path byte-identical.
    pub img_fit: Option<(f32, f32, f32, f32)>,
    /// visible sub-window (content rect ∩ padding box) when the fit mode
    /// overflows the box (`cover`/`none`): decode crops to it. None = no
    /// crop (the content fits, or `fill`).
    pub img_clip: Option<(f32, f32, f32, f32)>,
    /// form-control identity (input/textarea/button/select): None for
    /// every other box; carried into the laid output as a [`FormBox`]
    pub form: Option<FormCtl>,
    /// inline-level placement: the box sits on the surrounding text line
    /// (shrink-to-fit, vertically centered) instead of opening its own
    /// block line — set for UA form-control chrome (`<select>`/`<input>`/
    /// `<textarea>`) and for author `display: inline-block`; `block()` is
    /// the only flow that reads it
    pub inline: bool,
    pub children: Vec<BoxNode>,
    // layout output (absolute CSS px)
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// last line still has content (block flow paragraph-gap bookkeeping)
    pub open: bool,
    /// range of `runs` this subtree emitted during place() — phase-2
    /// repositioning shifts exactly it (nested ranges compose)
    pub run_range: (u32, u32),
    /// same for `underline_rects`
    pub ul_range: (u32, u32),
}

impl BoxNode {
    fn group(style: Style, bx: BoxStyle) -> Self {
        BoxNode {
            is_text: false,
            is_bg: false,
            is_mask: false,
            is_marker: false,
            bg_tile_region: None,
            style,
            bx,
            text: String::new(),
            hard_break: false,
            link: None,
            node: None,
            img: None,
            img_fit: None,
            img_clip: None,
            form: None,
            inline: false,
            children: Vec::new(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            open: false,
            run_range: (0, 0),
            ul_range: (0, 0),
        }
    }

    fn leaf(style: Style, text: String, hard_break: bool, link: Option<std::rc::Rc<str>>) -> Self {
        BoxNode {
            is_text: true,
            is_bg: false,
            is_mask: false,
            is_marker: false,
            bg_tile_region: None,
            style,
            bx: BoxStyle::default(),
            text,
            hard_break,
            link,
            node: None,
            img: None,
            img_fit: None,
            img_clip: None,
            form: None,
            inline: false,
            children: Vec::new(),
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
            open: false,
            run_range: (0, 0),
            ul_range: (0, 0),
        }
    }
}

/// Elements whose content is never rendered (also covers `<style>`/`<script>`
/// that appear inside body — head is already excluded by `body_root`).
const NON_RENDERED: &[&str] = &[
    "script", "style", "noscript", "template", "head", "title", "meta", "link",
];

/// Build the box tree for `node`'s subtree. `style`/`bx` are the cascade
/// result for `node` itself (main resolves body before calling).
pub fn build(dom: &Dom, node: u32, style: Style, bx: BoxStyle, sheet: &Sheet) -> BoxNode {
    // BoxStyle moves into the root below; the cursor seed threads down first
    let seed = bx.cursor;
    let mut root = BoxNode::group(style, bx);
    // identity for hover hit-testing — the root box is filtered out at
    // `collect_hover` (see its doc), this is only uniformity
    root.node = Some(node);
    // body's own background-image (root = body's box) — pushed before
    // build_into appends, so it stays the first child (paints under)
    if let Some(u) = root.bx.bg_image.clone() {
        let ph = bg_placeholder(&u, style, &root.bx);
        root.children.push(ph);
    }
    // body's own mask-image: second placeholder, same pipeline — a live
    // one replaces the body's own background at paint (mask wins)
    if let Some(u) = root.bx.mask_image.clone() {
        let ph = mask_placeholder(&u, style, &root.bx);
        root.children.push(ph);
    }
    // the body's own ::before/::after (the root IS the body's box):
    // ::before paints over the background placeholder (pushed after it)
    // and under the content; ::after closes the box. `bx.cursor` is the
    // body's effective cursor — build threads it down from here.
    if let Some(b) = pseudo_box(sheet, dom, node, style, None, seed, PseudoEl::Before) {
        root.children.push(b);
    }
    build_into(dom, node, style, sheet, None, seed, &mut root.children);
    if let Some(a) = pseudo_box(sheet, dom, node, style, None, seed, PseudoEl::After) {
        root.children.push(a);
    }
    root
}

/// Synthetic child for `background-image` — it rides the existing `<img>`
/// pipeline: `probe_images` (main.rs) resolves the raw url against the page
/// URL and probes the header, `decode_images` decodes at display size and
/// appends the draw entry in tree order — child 0 means the background
/// lands UNDER the element's content images, and all images draw under
/// text. `place()` sizes/positions it per the parent's `background-size`/
/// `background-position` and records the tiling region for a declared
/// `background-repeat`; `is_bg` keeps it out of every flow (block/flex/
/// grid and shrink-to-fit width).
/// shortcut: one image layer per box (extra `url()` layers dropped).
fn bg_placeholder(url: &str, style: Style, parent: &BoxStyle) -> BoxNode {
    let mut g = BoxNode::group(style, BoxStyle::default());
    g.is_bg = true;
    g.img = Some(ImgSlot::Pending(std::rc::Rc::from(url)));
    // the geometry longhands ride along on the placeholder so `place()`
    // and `collect_paint` can size/position/tile it without re-cascading
    g.bx.bg_size = parent.bg_size;
    g.bx.bg_pos = parent.bg_pos;
    g.bx.bg_repeat = parent.bg_repeat;
    g
}

/// Synthetic child for `mask-image` — [`bg_placeholder`] mechanics (same
/// shared `<img>` pipeline, same `place()` sizing/positioning/tiling math
/// against the padding box, no flow space), flagged `is_mask` so paint
/// treats it as the mask: it carries the MASK geometry longhands, and a
/// live one replaces its box's own background (see `BoxNode::is_mask`).
/// shortcut: one layer only — a multi-layer `mask:` list took its first
/// url at parse time.
fn mask_placeholder(url: &str, style: Style, parent: &BoxStyle) -> BoxNode {
    let mut g = bg_placeholder(url, style, parent);
    g.is_bg = false;
    g.is_mask = true;
    g.bx.bg_size = parent.mask_size;
    g.bx.bg_pos = parent.mask_pos;
    g.bx.bg_repeat = parent.mask_repeat;
    g
}

/// True when a box's style paints chrome (background, border, shadow,
/// horizontal padding) — an INLINE element with chrome gets a real inline
/// box instead of flattening into the parent flow, and a pseudo-element
/// box with chrome wraps its generated text in a box rather than a bare
/// run.
fn paints_chrome(bx: &BoxStyle) -> bool {
    let nz = |l: Len| !matches!(l, Len::Px(v) if v == 0.0);
    bx.bg.is_some()
        || bx.bg_image.is_some()
        || !bx.shadows.is_empty()
        || bx.border.iter().any(|&l| nz(l))
        || nz(bx.pad[1])
        || nz(bx.pad[3])
}

/// The `kind` pseudo-element box for `node`, or None when it generates no
/// box: the sheet has no such rules, `content` is undeclared/`none`/
/// `normal`, or the pseudo is `display:none`. `base` is the origin
/// element's computed style — inherited properties (color, fonts) flow
/// into the pseudo; its own rules cascade over that with initial box
/// properties (`Sheet::resolve_pseudo`).
///
/// The generated box is inline-level: a bare text run that flows with the
/// surrounding text, or an inline box carrying the pseudo's own style when
/// it paints chrome (background/border/padding) or declares a box-level
/// `display` — the same box decision an element with that style gets.
/// shortcut: generated content is plain text — non-list counters render as
/// nothing (list-item markers are a separate synthesized run, see
/// `list_marker`), and a `::before` on a replaced element (`<img>`/`<svg>`)
/// or form control generates no box at all (those branches return before
/// this is consulted).
fn pseudo_box(
    sheet: &Sheet,
    dom: &Dom,
    node: u32,
    base: Style,
    link: Option<std::rc::Rc<str>>,
    cursor: Option<Cursor>,
    kind: PseudoEl,
) -> Option<BoxNode> {
    let out = sheet.resolve_pseudo(dom, node, base, kind)?;
    if out.display_none {
        return None;
    }
    // effective cursor = the pseudo's own declaration over the origin's
    // (inherited) one; the generated box belongs to the ORIGIN element
    let cur = out.bx.cursor.or(cursor);
    let raw = out.bx.content.as_deref()?; // content undeclared → no box
    // `None` item: ::before/::after have no list-item counter
    let text = content_text(raw, dom, node, None)?; // none/normal/malformed → no box
    // generated content transforms like any other built text box
    let text = transform_text(&text, out.style.text_transform).into_owned();
    let chrome = paints_chrome(&out.bx);
    // same box decision as a tagless element: Auto/Inline flow as bare
    // text, inline-block and block-level displays get a real box
    let stacked = matches!(
        out.bx.display,
        Display::Block | Display::Flex | Display::Grid
    );
    let boxed = stacked || chrome || out.bx.display == Display::InlineBlock;
    if !boxed && text.is_empty() {
        return None; // nothing visible (e.g. a lone `content: counter(x)`)
    }
    if boxed {
        let mut g = BoxNode::group(out.style, out.bx);
        g.node = Some(node);
        g.bx.cursor = cur;
        g.inline = !stacked;
        let mut l = BoxNode::leaf(out.style, text, false, link);
        l.node = Some(node);
        l.bx.cursor = cur;
        g.children.push(l);
        Some(g)
    } else {
        let mut l = BoxNode::leaf(out.style, text, false, link);
        l.node = Some(node);
        l.bx.cursor = cur;
        Some(l)
    }
}

/// Synthesize the marker run for one list item. `ordinal` is the item's
/// number inside its parent list (build_into counts `<li>` siblings, honoring
/// `<ol start>`/`<li value>`). Returns None when `list-style-type` is `none`.
///
/// The marker is a text leaf carrying the marker text (`•`, `1.`, `iv.`, …)
/// in the item's own style, so it inherits color/font like real marker boxes.
/// A `::marker` rule overrides the text (`content`, with
/// `counter(list-item)` resolving to this item's number) and the color;
/// shortcut: no other `::marker` properties are applied (font/border/etc.),
/// and `content: none` keeps the UA marker rather than removing it.
fn list_marker(
    sheet: &Sheet,
    dom: &Dom,
    node: u32,
    ordinal: i64,
    base: Style,
    link: Option<std::rc::Rc<str>>,
    cursor: Option<Cursor>,
) -> Option<BoxNode> {
    let mut text = marker_text(base.list_type, ordinal)?;
    let mut style = base;
    if let Some(m) = sheet.resolve_pseudo(dom, node, base, PseudoEl::Marker) {
        if let Some(raw) = m.bx.content.as_deref() {
            if let Some(t) = content_text(raw, dom, node, Some((ordinal, base.list_type))) {
                if !t.is_empty() {
                    text = t;
                }
            }
        }
        // only `color` applies (see shortcut above); other properties stay
        // the item's inherited ones
        style.color = m.style.color;
    }
    // inside markers are ordinary first-line text: the space separates the
    // glyph from the content. Outside markers are positioned by `block()`,
    // which adds the gap itself.
    if base.list_pos == ListPos::Inside {
        text.push(' ');
    }
    let mut l = BoxNode::leaf(style, text, false, link);
    l.is_marker = true;
    l.node = Some(node);
    l.bx.cursor = cursor;
    Some(l)
}

/// `width="85%"` / `width="400"` presentational attribute → [`Len`] — the
/// same `Len` the CSS `width` property produces, so the attribute behaves
/// identically at layout.
fn attr_len(v: &str) -> Option<Len> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix('%') {
        return n.trim().parse::<f32>().ok().map(Len::Pct);
    }
    v.trim_end_matches("px")
        .trim()
        .parse::<f32>()
        .ok()
        .map(Len::Px)
}

/// Total node count (for the [mem] log).
pub fn count_boxes(n: &BoxNode) -> usize {
    1 + n.children.iter().map(count_boxes).sum::<usize>()
}

fn build_into(
    dom: &Dom,
    node: u32,
    style: Style,
    sheet: &Sheet,
    link: Option<std::rc::Rc<str>>,
    cursor: Option<Cursor>,
    out: &mut Vec<BoxNode>,
) {
    // Per-list item numbering: `node` is the PARENT whose children are being
    // built, so each `<ol>`/`<ul>` gets its own counter (nested lists restart
    // independently). `<ol start=N>` seeds it; `<li value=N>` resets it for
    // that item and the following ones continue from there. shortcut: the
    // `reversed` attribute is ignored.
    let mut list_counter = if dom.tag(node) == "ol" {
        dom.attr(node, "start")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(1)
            - 1
    } else {
        0
    };
    for &child in &dom.nodes[node as usize].children {
        match &dom.nodes[child as usize].kind {
            NodeKind::Text(t) => {
                let t = t.trim_matches('\u{a0}');
                if !t.is_empty() {
                    // text-transform lands HERE, when the text box is
                    // built: runs, wrapping, measurement and draw_text all
                    // see the final string (see transform_text)
                    let t = transform_text(t, style.text_transform);
                    // the containing element (not the text node) is the
                    // hover target; `cursor` is already its effective value
                    let mut l = BoxNode::leaf(style, t.into_owned(), false, link.clone());
                    l.node = Some(node);
                    l.bx.cursor = cursor;
                    out.push(l);
                }
            }
            NodeKind::Element(..) => {
                let tag = dom.tag(child);
                if NON_RENDERED.contains(&tag) {
                    continue;
                }
                // list item number (see the counter above). Computed before
                // the display:none skip so a hidden item still counts, like
                // real list numbering.
                let mut ordinal = 0i64;
                if tag == "li" {
                    list_counter = dom
                        .attr(child, "value")
                        .and_then(|v| v.trim().parse::<i64>().ok())
                        .unwrap_or(list_counter + 1);
                    ordinal = list_counter;
                }
                if tag == "br" || tag == "hr" {
                    out.push(BoxNode::leaf(style, String::new(), true, None));
                    continue;
                }
                let child_style = if tag == "body" { style } else { style_for(tag, style) };
                // author CSS (rules + inline style) cascades over UA defaults;
                // relative font-sizes compute against the TRUE parent (style),
                // not the UA-bumped child_style (headings would compound 2x)
                let outcome = sheet.resolve_with_parent(dom, child, child_style, dom.attr(child, "style"), style.size);
                if outcome.display_none {
                    continue;
                }
                // effective cursor for this element's box + subtree: its
                // own declaration over the inherited one (cursor inherits,
                // see BoxStyle::cursor)
                let cur = outcome.bx.cursor.or(cursor);
                if tag == "img" {
                    // replaced element: box now, pixels when main fetches the src.
                    // width/height attributes are presentational hints (CSS wins:
                    // outcome.bx already holds declared width/height).
                    let mut g = BoxNode::group(outcome.style, outcome.bx);
                    g.node = Some(child);
                    g.bx.cursor = cur;
                    if g.bx.width.is_none() {
                        g.bx.width = dom.attr(child, "width").and_then(|v| v.parse::<f32>().ok()).map(Len::Px);
                    }
                    if g.bx.height.is_none() {
                        g.bx.height = dom.attr(child, "height").and_then(|v| v.parse::<f32>().ok()).map(Len::Px);
                    }
                    if let Some(src) = dom.attr(child, "src").filter(|s| !s.is_empty()) {
                        g.img = Some(ImgSlot::Pending(std::rc::Rc::from(src)));
                    }
                    // author `inline-block` on a replaced box sits on the
                    // text line like any other inline-block
                    g.inline = g.bx.display == Display::InlineBlock;
                    out.push(g);
                    continue;
                }
                if tag == "svg" {
                    // inline <svg>: the parser kept the raw inner markup as a
                    // text child (svg is RAW_TEXT). Reassemble the full
                    // `<svg attrs>…</svg>` and hand it to the shared image
                    // pipeline (ImgSlot::Pending holding markup, not a URL).
                    let mut markup = String::from("<svg");
                    if let NodeKind::Element(_, attrs) = &dom.nodes[child as usize].kind {
                        for (k, v) in attrs.iter() {
                            markup.push_str(&format!(" {k}=\"{}\"", v.replace('"', "&quot;")));
                        }
                    }
                    markup.push('>');
                    for &gc in &dom.nodes[child as usize].children {
                        if let NodeKind::Text(t) = &dom.nodes[gc as usize].kind {
                            markup.push_str(t);
                        }
                    }
                    markup.push_str("</svg>");
                    // class-based paint: site CSS coloring this SVG's
                    // children by class rule (e.g. `.logo__letter { fill: …
                    // }`) — merged in as inline `style`, which the rasterizer
                    // prefers over presentation attributes. SVGs without
                    // matching single-class rules pass through unchanged.
                    let dark = sheet.is_dark(dom, child);
                    let markup = inject_svg_class_paint(&markup, sheet, dark);
                    // currentColor = the element's cascaded color (class
                    // rules like .text-next-blue already resolved via var())
                    let hex = format!("#{:06x}", outcome.style.color & 0x00ff_ffff);
                    let markup = markup
                        .replace("currentColor", &hex)
                        .replace("currentcolor", &hex);
                    let mut g = BoxNode::group(outcome.style, outcome.bx);
                    g.node = Some(child);
                    g.bx.cursor = cur;
                    if g.bx.width.is_none() {
                        g.bx.width = dom.attr(child, "width").and_then(|v| v.parse::<f32>().ok()).map(Len::Px);
                    }
                    if g.bx.height.is_none() {
                        g.bx.height = dom.attr(child, "height").and_then(|v| v.parse::<f32>().ok()).map(Len::Px);
                    }
                    g.img = Some(ImgSlot::Pending(std::rc::Rc::from(markup)));
                    g.inline = g.bx.display == Display::InlineBlock;
                    out.push(g);
                    continue;
                }
                // <a href> opens a link scope; nested <a> is invalid HTML so inherit-or-replace is enough
                let child_link = if tag == "a" {
                    dom.attr(child, "href")
                        .map(|h| std::rc::Rc::from(h))
                        .or_else(|| link.clone())
                } else {
                    link.clone()
                };
                // form controls: one inline box with UA chrome, content
                // collapsed to a single text run (the selected/first
                // <option> label, the input value, the textarea default,
                // the <button> label). Options are never recursed into
                // here — a closed <select> shows ONE line, never stacked
                // labels. Every box carries its FormCtl so layout can
                // collect FormBoxes for hit-testing/focus.
                if tag == "select" || tag == "input" || tag == "textarea" || tag == "button"
                {
                    // shortcut: multiple/size>1 selects and textarea
                    // rows/cols render like the closed single-line control —
                    // there is no popup/listbox/scrolling widget yet
                    let mut kind = FormKind::Select;
                    // raw stored value (never masked, never the arrow):
                    // value attr / textarea content / option label / button
                    // label — seeded into Layout::form_values at collect
                    let mut raw = String::new();
                    let (text, min_h, def_w, centered): (
                        Option<String>,
                        f32,
                        Option<f32>,
                        bool,
                    ) = match tag {
                        "select" => {
                            kind = FormKind::Select;
                            let mut opts: Vec<(String, bool)> = Vec::new();
                            option_labels(dom, child, &mut opts);
                            let label = opts
                                .iter()
                                .find(|(_, s)| *s)
                                .or_else(|| opts.first())
                                .map(|(l, _)| l.clone());
                            raw = label.clone().unwrap_or_default();
                            // dropdown arrow appended to the one visible
                            // line — shortcut: a text glyph, not a
                            // rasterizer-drawn triangle. ▼ (U+25BC) not ▾:
                            // the small triangle U+25BE is absent from the
                            // whole system-font fallback chain (verified via
                            // ab_glyph glyph_id), ▼ is in every one of them
                            // no options → empty box (no text, no arrow), no crash
                            (label.map(|l| format!("{l} \u{25bc}")), 20.0, None, false)
                        }
                        "textarea" => {
                            kind = FormKind::TextInput { password: false };
                            let t = contained_text(dom, child);
                            raw = t.clone();
                            (if t.is_empty() { None } else { Some(t) }, 20.0, Some(170.0), false)
                        }
                        "button" => {
                            // <button>: label = its text content; default
                            // type is submit (activate on click). The label
                            // is centered in the box like real UA buttons.
                            kind = FormKind::Submit;
                            let t = contained_text(dom, child);
                            raw = t.clone();
                            (if t.is_empty() { None } else { Some(t) }, 20.0, None, true)
                        }
                        _ => {
                            // input (VOID: value comes from the attribute)
                            let ty = dom.attr(child, "type").unwrap_or("").trim().to_ascii_lowercase();
                            if ty == "hidden" {
                                continue; // never rendered, by spec
                            }
                            kind = match ty.as_str() {
                                "checkbox" => FormKind::Checkbox,
                                "radio" => FormKind::Radio,
                                "submit" | "button" | "reset" | "image" => FormKind::Submit,
                                "password" => FormKind::TextInput { password: true },
                                // other typed inputs: chrome box, no widget
                                // art and no interaction yet
                                "range" | "color" | "date" | "time" | "month" | "week"
                                | "datetime-local" | "file" => FormKind::Generic,
                                _ => FormKind::TextInput { password: false },
                            };
                            // raw stored value = the value attribute only
                            // (placeholder is display fallback, never a value)
                            raw = dom.attr(child, "value").unwrap_or("").to_string();
                            let val = dom
                                .attr(child, "value")
                                // no value → the placeholder shows as static text
                                .or_else(|| dom.attr(child, "placeholder"))
                                .unwrap_or("");
                            // shortcut: checkbox/radio/range/color/image
                            // render as a generic chrome box — no widget art
                            let body = match kind {
                                FormKind::Checkbox | FormKind::Radio => String::new(),
                                FormKind::TextInput { password: true } => {
                                    "\u{2022}".repeat(val.chars().count())
                                }
                                FormKind::TextInput { password: false } => val.to_string(),
                                // button-ish inputs show their value as the label
                                FormKind::Submit => val.to_string(),
                                FormKind::Select => String::new(),
                                // range/color/date/… show the value attr as text
                                FormKind::Generic => val.to_string(),
                            };
                            let text = if body.is_empty() { None } else { Some(body) };
                            let (min_h, def_w) = match kind {
                                // 14x14 OUTER: min-height is content-box in
                                // this engine (width is border-box), so the
                                // UA 1px border (×2) must be subtracted
                                FormKind::Checkbox | FormKind::Radio => (12.0, Some(14.0)),
                                // text-ish inputs: the classic ~20ch (~170px)
                                // default width whether empty or filled —
                                // longer values are tail-clipped at the box
                                // edge during layout (see layout_in_forms)
                                FormKind::TextInput { .. } => (20.0, Some(170.0)),
                                // button-ish inputs shrink to their label
                                FormKind::Submit => (20.0, None),
                                FormKind::Select => (20.0, None),
                                // generic chrome box: shrink to its value text
                                FormKind::Generic => (20.0, None),
                            };
                            let centered = matches!(kind, FormKind::Submit);
                            (text, min_h, def_w, centered)
                        }
                    };
                    let mut g = BoxNode::group(outcome.style, outcome.bx);
                    control_chrome(&mut g.bx, min_h, def_w);
                    g.node = Some(child);
                    g.bx.cursor = cur;
                    // UA default (no author `display` declared) = the control
                    // joins the surrounding text line; an author
                    // `display:inline-block` keeps that placement (inline-level
                    // in real engines too); `display:block/flex/grid` opts out
                    // and stacks.
                    g.inline = matches!(g.bx.display, Display::Auto | Display::InlineBlock);
                    g.form = Some(FormCtl { node: child, kind, value: raw });
                    if let Some(t) = text {
                        let mut st = outcome.style;
                        // button labels center in their box (UA default)
                        if centered {
                            st.text_align = 1;
                        }
                        // shortcut: form labels/values skip text-transform
                        // (they show live user input; case-mapping a value
                        // the user typed would fight the control itself)
                        let mut l = BoxNode::leaf(st, t, false, child_link.clone());
                        l.node = Some(child);
                        l.bx.cursor = cur;
                        g.children.push(l);
                    }
                    out.push(g);
                    continue;
                }
                // ::before/::after generated content — cascaded once here
                // against this element's computed style (attr() needs the
                // DOM), inserted as its first/last child in the flow
                // branches below. The <img>/<svg>/form-control branches
                // above return before this point: replaced elements and
                // controls generate no pseudo content (see pseudo_box).
                let before = pseudo_box(
                    sheet,
                    dom,
                    child,
                    outcome.style,
                    child_link.clone(),
                    cur,
                    PseudoEl::Before,
                );
                let after = pseudo_box(
                    sheet,
                    dom,
                    child,
                    outcome.style,
                    child_link.clone(),
                    cur,
                    PseudoEl::After,
                );
                // which elements get a box: flex/grid/block/inline-block
                // always; inline tags only when CSS says so; Auto follows
                // the tag. Absolutely/fixed positioned elements ALWAYS get
                // one — their display blockifies to block (otherwise the
                // skip-link pattern `a{position:absolute}` would flatten
                // into the parent flow and ignore its insets/clip); floated
                // elements likewise blockify (a floated inline would
                // otherwise flatten away and lose its float).
                let boxed = match outcome.bx.display {
                    Display::Auto => {
                        is_block(tag)
                            || matches!(outcome.bx.pos, Pos::Absolute | Pos::Fixed)
                            || outcome.bx.float != Float::None
                    }
                    Display::Inline => {
                        matches!(outcome.bx.pos, Pos::Absolute | Pos::Fixed)
                            || outcome.bx.float != Float::None
                    }
                    _ => true,
                };
                if boxed {
                    let mut g = BoxNode::group(outcome.style, outcome.bx);
                    g.node = Some(child);
                    g.bx.cursor = cur;
                    // table presentational hints (lower precedence than
                    // author CSS): width="85%"/"400" behaves exactly like
                    // the CSS property; align="center" centers the band
                    // like `margin: auto`
                    if tag == "table" {
                        if g.bx.declared & decl::WIDTH == 0 {
                            g.bx.width = dom.attr(child, "width").and_then(attr_len);
                        }
                        let center = dom
                            .attr(child, "align")
                            .map_or(false, |a| a.trim().eq_ignore_ascii_case("center"));
                        if center && !g.bx.margin_x_auto && g.bx.margin == [Len::Px(0.0); 4] {
                            g.bx.margin_x_auto = true;
                        }
                    }
                    // background-image: placeholder rides the <img> pipeline
                    // and paints under this box's content (first child)
                    if let Some(u) = g.bx.bg_image.clone() {
                        let ph = bg_placeholder(&u, outcome.style, &g.bx);
                        g.children.push(ph);
                    }
                    // mask-image: second placeholder on the same pipeline;
                    // a live one replaces this box's own background at paint
                    if let Some(u) = g.bx.mask_image.clone() {
                        let ph = mask_placeholder(&u, outcome.style, &g.bx);
                        g.children.push(ph);
                    }
                    // list-item marker — a synthesized run BEFORE all content
                    // (marker box first, then ::before, then the item).
                    // shortcut: markers are generated for `<li>` elements
                    // only (the UA `display:list-item` equivalent here);
                    // an author `display:list-item` on another tag gets no
                    // marker.
                    if tag == "li" {
                        if let Some(m) = list_marker(
                            sheet, dom, child, ordinal, outcome.style, child_link.clone(), cur,
                        ) {
                            g.children.push(m);
                        }
                    }
                    if let Some(b) = before {
                        g.children.push(b);
                    }
                    // inline-block: a real box (background/border paint),
                    // but placed ON the surrounding text line by block()'s
                    // inline branch instead of stacking
                    g.inline = g.bx.display == Display::InlineBlock;
                    build_into(dom, child, outcome.style, sheet, child_link, cur, &mut g.children);
                    if let Some(a) = after {
                        g.children.push(a);
                    }
                    out.push(g);
                } else {
                    // An inline element that PAINTS (background, border,
                    // box-shadow) or carries horizontal padding gets a real
                    // inline-level box. Flattening would drop the band, and a
                    // box shrink-wrapping its text + padding is exactly the
                    // band real engines draw (e.g. an inline <a> styled as a
                    // button). Same placement path as author inline-block.
                    if paints_chrome(&outcome.bx) {
                        let mut g = BoxNode::group(outcome.style, outcome.bx);
                        g.node = Some(child);
                        g.bx.cursor = cur;
                        g.inline = true;
                        // background-image placeholder (see above)
                        if let Some(u) = g.bx.bg_image.clone() {
                            let ph = bg_placeholder(&u, outcome.style, &g.bx);
                            g.children.push(ph);
                        }
                        // mask placeholder (see above)
                        if let Some(u) = g.bx.mask_image.clone() {
                            let ph = mask_placeholder(&u, outcome.style, &g.bx);
                            g.children.push(ph);
                        }
                        if let Some(b) = before {
                            g.children.push(b);
                        }
                        build_into(dom, child, outcome.style, sheet, child_link, cur, &mut g.children);
                        if let Some(a) = after {
                            g.children.push(a);
                        }
                        out.push(g);
                    } else {
                        // plain inline (no paint chrome): children flatten
                        // into the parent flow — UNLESS the element carries
                        // horizontal margins. Those must advance the line by
                        // their real width: emitting one literal space (the old
                        // approximation) left adjacent inline boxes jammed or
                        // overlapping. Promote to an inline-edge box (same
                        // placement path as the paint branch above), so
                        // block()'s inline branch lays out ml + width + mr.
                        // Collapse semantics: inline margins never collapse —
                        // CSS collapses only block-level boxes; an inline edge
                        // margin is always real advance, and adjacent boxes'
                        // margins simply add on the line.
                        let has_margin = |l: Len| !matches!(l, Len::Px(v) if v == 0.0);
                        if has_margin(outcome.bx.margin[3]) || has_margin(outcome.bx.margin[1]) {
                            let mut g = BoxNode::group(outcome.style, outcome.bx);
                            g.node = Some(child);
                            g.bx.cursor = cur;
                            g.inline = true;
                            if let Some(b) = before {
                                g.children.push(b);
                            }
                            build_into(dom, child, outcome.style, sheet, child_link, cur, &mut g.children);
                            if let Some(a) = after {
                                g.children.push(a);
                            }
                            out.push(g);
                        } else {
                            if let Some(b) = before {
                                out.push(b);
                            }
                            build_into(dom, child, outcome.style, sheet, child_link, cur, out);
                            if let Some(a) = after {
                                out.push(a);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One painted `box-shadow` — resolved to px at layout. `color` already
/// carries alpha (0xAARRGGBB); drawn as a blurred rounded rect.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ShadowRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
    /// blur radius (0 = hard edge)
    pub blur: f32,
    /// packed alpha × color
    pub color: u32,
    pub fixed: bool,
}

pub struct PositionedRun {
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub style: Style,
    pub width: f32,
    pub link: Option<std::rc::Rc<str>>,
    /// inside a `position: fixed` subtree — drawn in viewport coordinates
    /// (no scroll offset), on top of scrolled content
    pub fixed: bool,
    /// accumulated clip window (x0, y0, x1, y1 — doc coords) from ancestor
    /// `overflow`/`clip: rect()` boxes; None = unclipped. Set by
    /// [`apply_run_clips`] after layout; nested clips compose by
    /// intersection. An empty window (x0 ≥ x1) means main draws nothing.
    pub clip: Option<(f32, f32, f32, f32)>,
    /// DOM element this run's text belongs to + its effective `cursor` —
    /// hover hit-testing only (worker-internal: the wire conversion drops
    /// them, [`Layout::hover_targets`] carries the results upstream)
    pub node: Option<u32>,
    pub cursor: Option<Cursor>,
}

/// One hover hit-test rectangle: document-coord rect + the DOM element it
/// belongs to + the effective `cursor` + the `position: fixed` flag.
/// Built by `collect_hover` in tree order; reverse-scanned by
/// [`Layout::hit_hover`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct HoverTarget {
    /// (x, y, w, h) in document coordinates
    pub rect: (f32, f32, f32, f32),
    pub node: u32,
    pub cursor: Option<Cursor>,
    pub fixed: bool,
}

/// One `position: sticky` box's paint-time pinning record — see
/// [`Layout::sticky`]. Layout leaves the box at its static flow position
/// (the scroll threshold never offsets flow, unlike `position: relative`);
/// each frame `render()` adds [`sticky_offset`] to every paint entry of the
/// box's ENTIRE subtree so the whole subtree moves together, keeping the
/// z order `zsort` already gave it (the shift never re-sorts — sticky vs
/// `z-index` interactions are approximate: a pinned entry keeps painting at
/// its sorted level while it moves).
///
/// Spans are `(start, end)` ranges into the paint channels: `rects`/
/// `shadows`/`imgs` from `collect_paint`'s push indices (a subtree's
/// entries are contiguous in the DFS), `runs`/`uls` from the box's
/// `run_range`/`ul_range`; `imgs_dec` covers the content images the shell
/// decodes AFTER layout (`main.rs` `decode_images` appends them past every
/// z-sorted entry).
///
/// shortcut: only the `top` threshold pins — `bottom`/`left`/`right`
/// never do (top pinning is what real site headers/sidebars declare);
/// `top` in `%` resolves against the parent box's height; `container_y1`
/// is the PARENT box's border-box bottom, not the spec's nearest
/// scrollport containing block; nested/overlapping sticky groups shift
/// independently (an entry inside two groups' spans gets both offsets —
/// approximation for a sticky box inside a sticky box, rare); form rects
/// and hit-testing are NOT shifted (`FormBox`, the link walk) — hover and
/// click on a stuck navbar land where the box was before pinning, and a
/// form control inside a sticky subtree paints at its flow position; no
/// re-clipping at the container edge (the clamp keeps the group inside
/// its container, so entries can't smear past it, but entries pre-culled
/// against an ANCESTOR clip keep their flow-position cut — while a sticky
/// box's own run clip moves with the group, glued like a transform); an
/// absolute/fixed descendant sits outside `run_range`/`ul_range` and
/// keeps its own position; inside a `position: fixed` subtree the
/// coordinates don't scroll, so pinning runs against scroll 0 (`fixed`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct StickyGroup {
    /// (start, end) into `Layout::shadows`
    pub shadows: (usize, usize),
    /// (start, end) into `Layout::rects`
    pub rects: (usize, usize),
    /// (start, end) into `Layout::images` from layout paint
    pub imgs: (usize, usize),
    /// (start, end) into `Layout::images` from the post-layout decode
    /// pass (empty until `main.rs` fills it)
    pub imgs_dec: (usize, usize),
    /// (start, end) into `Layout::runs`
    pub runs: (usize, usize),
    /// (start, end) into `Layout::underline_rects`
    pub uls: (usize, usize),
    /// the sticky box's own flow y (border box, doc coords)
    pub doc_y: f32,
    /// its flow height
    pub doc_h: f32,
    /// resolved `top` threshold in px (`BoxStyle::inset[0]`; only boxes
    /// that declare `top` get a group — a threshold-less sticky never pins)
    pub top: f32,
    /// parent box's border-box bottom — the edge the pin clamps against
    pub container_y1: f32,
    /// inside a `position: fixed` subtree: coordinates are viewport-
    /// relative, so pinning ignores scroll (see struct doc)
    pub fixed: bool,
}

/// One frame's pin offset for one sticky group (see [`StickyGroup`]):
/// `0.0` while the box's flow top still sits below `scroll + top`
/// (un-stuck — layout is never touched), then it follows `scroll + top`
/// (the box's document y becomes `scroll + top`, i.e. it pins `top` px
/// below the viewport top), clamped so the box's bottom never passes
/// `container_y1`. A container shorter than the box gives a negative
/// clamp range → `max(0.0)` → never pins. In a `fixed` subtree nothing
/// scrolls, so the threshold runs against scroll 0; a sticky box inside
/// a plain `absolute` subtree keeps document coordinates, so the normal
/// scroll path applies to it unchanged.
pub fn sticky_offset(g: &StickyGroup, scroll: f32) -> f32 {
    let scroll = if g.fixed { 0.0 } else { scroll };
    let room = g.container_y1 - (g.doc_y + g.doc_h);
    (scroll + g.top - g.doc_y).clamp(0.0, room.max(0.0))
}

/// Per-frame sticky pin deltas for every paint entry: `entry.y +
/// off[channel][i]` is the pinned y. All five vectors are empty on pages
/// without sticky boxes, so every lookup yields `0.0` and paint is
/// byte-identical to a build without the feature.
#[derive(Default)]
pub struct StickyFrame {
    pub sh: Vec<f32>,
    pub rect: Vec<f32>,
    pub img: Vec<f32>,
    pub run: Vec<f32>,
    pub ul: Vec<f32>,
}

/// Build this frame's [`StickyFrame`] from the layout's groups (see
/// [`sticky_offset`]); entries covered by several groups (nested sticky)
/// get the sum of their offsets.
pub fn sticky_frame(laid: &Layout, scroll: f32) -> StickyFrame {
    if laid.sticky.is_empty() {
        return StickyFrame::default();
    }
    let mut f = StickyFrame {
        sh: vec![0.0; laid.shadows.len()],
        rect: vec![0.0; laid.rects.len()],
        img: vec![0.0; laid.images.len()],
        run: vec![0.0; laid.runs.len()],
        ul: vec![0.0; laid.underline_rects.len()],
    };
    for g in &laid.sticky {
        let d = sticky_offset(g, scroll);
        if d == 0.0 {
            continue;
        }
        add_span(&mut f.sh, g.shadows, d);
        add_span(&mut f.rect, g.rects, d);
        add_span(&mut f.img, g.imgs, d);
        add_span(&mut f.img, g.imgs_dec, d);
        add_span(&mut f.run, g.runs, d);
        add_span(&mut f.ul, g.uls, d);
    }
    f
}

/// Add `d` to every entry of one recorded span (out-of-range ends are
/// clamped — a span past the channel's end would be a bookkeeping bug,
/// and shifting what exists beats panicking mid-frame).
fn add_span(v: &mut [f32], (a, b): (usize, usize), d: f32) {
    let a = a.min(v.len());
    let b = b.min(v.len()).max(a);
    for x in &mut v[a..b] {
        *x += d;
    }
}

pub struct Layout {
    pub runs: Vec<PositionedRun>,
    pub height: f32,
    /// x, y, w, h, color (matches the run's text color), fixed
    pub underline_rects: Vec<(f32, f32, f32, f32, u32, bool)>,
    /// filled box backgrounds (x, y, w, h, color, radius, fixed) — drawn under text
    pub rects: Vec<(f32, f32, f32, f32, u32, f32, bool)>,
    /// decoded `<img>` boxes (x, y, w, h, pixels, fixed) — drawn under text
    pub images: Vec<(f32, f32, f32, f32, std::rc::Rc<ImageData>, bool)>,
    /// box-shadows (under backgrounds), paint order = list order
    pub shadows: Vec<ShadowRect>,
    /// true if the last line still has content on it (no trailing <br>) —
    /// block flow uses this to decide whether to add the paragraph gap
    pub open_line: bool,
    /// laid form controls in tree order (see [`FormBox`]) — hit-test
    /// targets for the shell
    pub forms: Vec<FormBox>,
    /// focus slot echoed from the [`FormState`] this layout was built
    /// with (DOM node id; None = nothing focused)
    pub focused: Option<u32>,
    /// raw (unmasked) values by DOM node id — live value if the shell set
    /// one via [`Layout::set_form_value`], else the initial value from
    /// the markup
    pub form_values: HashMap<u32, String>,
    /// one entry per `position: sticky` box that declares `top` — the
    /// paint-time pinning records the shell applies per frame (see
    /// [`StickyGroup`]); empty on pages without one. shortcut: boxes
    /// without `top` never pin, so they get no group.
    pub sticky: Vec<StickyGroup>,
    /// hover hit-test targets in tree order (element boxes + their own
    /// text runs) — reverse-scanned by [`Layout::hit_hover`]
    pub hover_targets: Vec<HoverTarget>,
}

/// Default viewport height for [`layout_tree`] — matches the fixed window in
/// main(); production passes the real one through [`layout_in`] (tests don't
/// depend on it).
#[cfg(test)]
const VIEWPORT_H: f32 = 676.0;

/// Background-image decode budget (pixels), separate from and much smaller
/// than the content `IMG_PIXEL_BUDGET` (64 MB, main.rs): backgrounds are
/// decorative, so any placeholder whose probed intrinsic size doesn't fit
/// the remaining budget loses its slot before layout (no decode, no draw).
/// 256K px ≈ 1 MB RGBA — keeps the project's minimal-RAM metric.
const BG_PIXEL_BUDGET: usize = 256 * 1024;

/// Cap on the tile grid (cols × rows) one repeating background-image
/// placeholder may emit at paint. shortcut: RAM/paint discipline — a
/// pathological 1px tile inside a huge box degrades to a partial fill
/// instead of pushing hundreds of thousands of rects per frame; real
/// pattern strips are far under this.
const BG_TILE_CAP: i64 = 512;

/// Drop (de-slot) background-image and mask placeholders whose probed
/// intrinsic pixels don't fit the remaining [`BG_PIXEL_BUDGET`] (both ride
/// the same budget — mask pixels are decorative like background pixels).
/// Content `<img>` boxes are never touched — they keep main's shared
/// budget discipline.
fn cap_bg_images(n: &mut BoxNode, budget: &mut usize) {
    if n.is_bg || n.is_mask {
        if let Some(ImgSlot::Probed { w, h, .. }) = &n.img {
            let px = (*w as usize).saturating_mul(*h as usize);
            if px > *budget {
                n.img = None;
            } else {
                *budget -= px;
            }
        }
        return;
    }
    for c in &mut n.children {
        cap_bg_images(c, budget);
    }
}

/// Copy of `img` with every pixel's RGB replaced by `c` (0xRRGGBB) and the
/// alpha channel kept — the mask's shape painted in the box's background
/// color. Shared by the layout-time tint pass and main's decode path.
pub fn tint_image(img: &ImageData, c: u32) -> ImageData {
    let rep = c & 0x00FF_FFFF;
    ImageData {
        w: img.w,
        h: img.h,
        px: img.px.iter().map(|&p| (p & 0xFF00_0000) | rep).collect(),
    }
}

/// Tint every DECODED mask placeholder with its box's background color
/// (RGB replaced, alpha kept; no bg color → the mask stays untinted) and
/// cache the result back over the Ready slot — paint then reuses the
/// tinted pixels instead of re-tinting per frame, and the original buffer
/// is dropped with the overwritten Rc. Re-running is idempotent (the RGB
/// is replaced with the same color), so a re-layout is safe. Production
/// fills its slots post-layout, so main's `decode_images` applies the
/// same tint at the moment it decodes (see `tint_image`).
fn tint_masks(n: &mut BoxNode) {
    let tint = n.bx.bg;
    if let Some(c) = tint {
        for ch in &mut n.children {
            if ch.is_mask {
                if let Some(ImgSlot::Ready(d)) = &ch.img {
                    let tinted = tint_image(d, c);
                    ch.img = Some(ImgSlot::Ready(std::rc::Rc::new(tinted)));
                }
            }
        }
    }
    for ch in &mut n.children {
        tint_masks(ch);
    }
}

/// Lay out the box tree at (0, 0) within `max_width`, viewport height
/// [`VIEWPORT_H`] (see [`layout_in`] for the real one).
#[cfg(test)]
pub fn layout_tree(
    root: &mut BoxNode,
    max_width: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
) -> Layout {
    layout_in(root, max_width, VIEWPORT_H, measure)
}

/// Like [`layout_tree`] but with the real viewport height — `position: fixed`
/// boxes resolve `bottom`/`right` insets against (0, 0, max_width, vh).
/// No form state (nothing focused, values = the markup defaults) — see
/// [`layout_in_forms`] for the interactive entry point.
pub fn layout_in(
    root: &mut BoxNode,
    max_width: f32,
    viewport_h: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
) -> Layout {
    layout_in_forms(root, max_width, viewport_h, measure, &FormState::default())
}

/// [`layout_in`] plus form-control interaction state: `state.focused`
/// paints the accent border + caret on the focused text field,
/// `state.values` overrides what each control shows (password fields
/// mask for display only), and the result carries every control's laid
/// rect in [`Layout::forms`] for hit-testing.
///
/// Shell integration sketch (main.rs):
/// ```ignore
/// // Page { …, forms: FormState } — carried across re-layouts
/// // click:
/// if let Some(i) = lay.hit_test_form(mx, my, scroll_y) {
///     let fb = lay.forms[i];
///     if fb.kind.activates() {
///         // shortcut: no form POST — log and move on
///         println!("[form] submit {}={}", dom.attr(fb.node, "name").unwrap_or(""), lay.form_value(fb.node));
///     } else if fb.kind.takes_text() {
///         page.forms.focused = Some(fb.node); // DOM node id
///     }
/// } else { page.forms.focused = None; } // click on empty space blurs
/// // key into the focused field:
/// if let Some(n) = page.forms.focused {
///     let mut v = lay.form_value(n).to_string(); v.push(ch);
///     page.forms.values.insert(n, v);
/// }
/// // after any state change, re-lay out (fast — layout is cheap):
/// lay = layout_in_forms(&mut root, max_w, vh, &measure, &page.forms);
/// ```
pub fn layout_in_forms(
    root: &mut BoxNode,
    max_width: f32,
    viewport_h: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
    state: &FormState,
) -> Layout {
    let mut runs = Vec::new();
    let mut uls = Vec::new();
    // background-image budget: over-budget placeholders lose their slot
    // before anything is sized or painted (probe already ran in main)
    let mut bg_budget = BG_PIXEL_BUDGET;
    cap_bg_images(root, &mut bg_budget);
    // mask placeholders that already carry pixels get tinted with their
    // box's background color once, cached over the slot (see tint_masks)
    tint_masks(root);
    // apply the live form state to the control boxes BEFORE layout so the
    // shown text (value override, password mask, tail clip) sizes lines
    apply_form_state(root, state, measure);
    // initial containing block = viewport (0, 0, max_width, vh)
    let cb = (0.0f32, 0.0f32, max_width, viewport_h);
    place(root, 0.0, 0.0, max_width, measure, &mut runs, &mut uls, &[]);
    // phase 2: resolve relative/absolute/fixed against their containing
    // blocks now that every in-flow position is final (root flow box = cb)
    position_all(root, cb, cb, cb, false, measure, &mut runs, &mut uls);
    // clip runs/underline rects to ancestor overflow + clip:rect windows
    // (after position_all so shifts are final; ranges are complete)
    apply_run_clips(root, None, &mut runs, &mut uls);
    // hover targets: box rects + each box's own runs. Before zsort — the
    // stable sort would invalidate the run_range indices this walks.
    let mut hover_targets = Vec::new();
    collect_hover(
        root,
        &runs,
        false,
        root.node.unwrap_or(0),
        &mut hover_targets,
    );
    let mut rects = Vec::new();
    let mut rect_z = Vec::new();
    let mut images = Vec::new();
    let mut img_z = Vec::new();
    let mut shadows = Vec::new();
    let mut sticky = Vec::new();
    collect_paint(
        root,
        false,
        Xf::IDENT,
        0,
        None,
        // the root's "parent" is the viewport: a root-level sticky pins
        // against the initial containing block (its height)
        (0.0, viewport_h),
        &mut rects,
        &mut rect_z,
        &mut images,
        &mut img_z,
        &mut shadows,
        &mut sticky,
    );
    // stacking levels: each channel paints in (z, tree order) — a stable
    // sort keyed by the stamped levels (identity when no page sets z-index);
    // sticky spans were recorded in paint-walk order, so they follow their
    // entries through a permutation (exact while the subtree kept one level)
    let rm = zsort(&mut rects, &rect_z);
    let im = zsort(&mut images, &img_z);
    // text channel: move runs/underlines through their boxes' transforms
    // (layout coords never move — transforms are visual-only), stamp their
    // stacking levels, then stable-sort like the rect/image channels
    let mut run_z = vec![0i32; runs.len()];
    transform_runs(root, Xf::IDENT, 0, &mut runs, &mut uls, &mut run_z);
    let mm = zsort(&mut runs, &run_z);
    if !sticky.is_empty() {
        if let Some(m) = rm {
            remap_sticky_spans(&mut sticky, |g| &mut g.rects, &m);
        }
        if let Some(m) = im {
            remap_sticky_spans(&mut sticky, |g| &mut g.imgs, &m);
        }
        if let Some(m) = mm {
            remap_sticky_spans(&mut sticky, |g| &mut g.runs, &m);
        }
    }
    // form controls: laid rects + focus visuals (caret) + value seeding
    // (the caret is appended past every group's ul span — forms are never
    // shifted, see StickyGroup)
    let mut forms = Vec::new();
    let mut form_values = HashMap::new();
    collect_forms(root, false, state, &mut forms, &mut uls, &mut form_values, measure);
    Layout {
        runs,
        height: root.h,
        underline_rects: uls,
        rects,
        images,
        shadows,
        open_line: root.open,
        forms,
        focused: state.focused,
        form_values,
        sticky,
        hover_targets,
    }
}

/// UA accent blue — the focused text-field border/caret color (matches
/// the window chrome's accent).
const FORM_ACCENT: u32 = 0x00_6a_9c_ff;

/// Pre-layout walk over the box tree: for every form control, show the
/// live value from the state (password fields mask it for display only),
/// tail-clip text that would overflow the box, and paint the focus
/// border. Runs before [`place`] so line sizing sees the final text.
fn apply_form_state(
    n: &mut BoxNode,
    state: &FormState,
    measure: &dyn Fn(&str, &Style) -> f32,
) {
    if let Some((node, kind)) = n.form.as_ref().map(|c| (c.node, c.kind)) {
        let focused = state.focused == Some(node);
        // focus ring: the focused text field gets the accent border,
        // every other text field the neutral gray (author-declared
        // border colors always win — control_chrome only filled the UA
        // gray in when the author said nothing)
        if kind.takes_text() && n.bx.declared & decl::BCOLOR == 0 {
            n.bx.bcolor = Some(if focused { FORM_ACCENT } else { 0x00_76_76_76 });
        }
        // live value replaces what the box shows; password fields mask
        // (display only — storage keeps the raw text)
        if let Some(v) = state.values.get(&node) {
            let display = match kind {
                FormKind::TextInput { password: true } => "\u{2022}".repeat(v.chars().count()),
                _ => v.clone(),
            };
            // replace the text leaf's content (or add one when the field
            // started empty); link chrome is irrelevant on a value leaf
            if let Some(leaf) = n.children.iter_mut().find(|c| c.is_text) {
                leaf.text = display;
            } else if !display.is_empty() {
                n.children.push(BoxNode::leaf(n.style, display, false, None));
            }
        }
        // tail-clip text-ish values wider than the box's content area —
        // covers live overrides AND long build-time values (the box
        // keeps its ~170px default width, the value loses its tail).
        // Only when the box width is known in px (UA default or author
        // px width); percentage widths need layout first (shortcut:
        // no clip — the text just overflows its line).
        if kind.takes_text() {
            if let Some(Len::Px(w)) = n.bx.width {
                // content-box: the declared width IS the content area
                let content = if n.bx.content_box {
                    w
                } else {
                    w - resolve_len(n.bx.pad[1], w)
                        - resolve_len(n.bx.pad[3], w)
                        - resolve_len(n.bx.border[1], w)
                        - resolve_len(n.bx.border[3], w)
                };
                if let Some(leaf) = n.children.iter_mut().find(|c| c.is_text) {
                    // + tracking so a letter-spaced value clips at the same
                    // edge it paints (spacing_extra = the draw-side rule)
                    while !leaf.text.is_empty()
                        && measure(&leaf.text, &leaf.style) + spacing_extra(&leaf.text, &leaf.style)
                            > content
                    {
                        leaf.text.pop();
                    }
                }
            }
        }
    }
    for c in &mut n.children {
        apply_form_state(c, state, measure);
    }
}

/// Post-layout walk: collect every form control's laid border-box rect
/// (for [`Layout::hit_test_form`]) and its initial/raw value (seeded
/// into the value map, live overrides win). The focused text field also
/// gets a caret — a 1px vertical line at the end of its shown text,
/// carried in the underline-rects channel main already paints.
fn collect_forms(
    n: &BoxNode,
    fixed: bool,
    state: &FormState,
    out: &mut Vec<FormBox>,
    uls: &mut Vec<(f32, f32, f32, f32, u32, bool)>,
    values: &mut HashMap<u32, String>,
    measure: &dyn Fn(&str, &Style) -> f32,
) {
    if !n.is_text {
        let fixed = fixed || n.bx.pos == Pos::Fixed;
        if let Some(ctl) = &n.form {
            // seed the value map: the live state value wins over the
            // markup's initial value
            let v = state
                .values
                .get(&ctl.node)
                .cloned()
                .unwrap_or_else(|| ctl.value.clone());
            values.entry(ctl.node).or_insert(v);
            let focused = state.focused == Some(ctl.node);
            out.push(FormBox {
                node: ctl.node,
                rect: (n.x, n.y, n.w, n.h),
                kind: ctl.kind,
                focused,
                fixed,
            });
            // caret: 1px line at the text end, inside the content box
            if focused && ctl.kind.takes_text() {
                let bt = resolve_len(n.bx.border[0], n.w);
                let bl = resolve_len(n.bx.border[3], n.w);
                let pt = resolve_len(n.bx.pad[0], n.w);
                let pb = resolve_len(n.bx.pad[2], n.w);
                let pl = resolve_len(n.bx.pad[3], n.w);
                let text_w = n
                    .children
                    .iter()
                    .find(|c| c.is_text)
                    // + tracking: the caret sits after the painted ink
                    .map(|c| measure(&c.text, &c.style) + spacing_extra(&c.text, &c.style))
                    .unwrap_or(0.0);
                let caret_h = (n.h - bt - resolve_len(n.bx.border[2], n.w) - pt - pb).max(1.0);
                uls.push((
                    n.x + bl + pl + text_w,
                    n.y + bt + pt,
                    1.0,
                    caret_h,
                    FORM_ACCENT,
                    fixed,
                ));
            }
        }
    }
    for c in &n.children {
        collect_forms(c, fixed, state, out, uls, values, measure);
    }
}

/// Collect every hover hit-test rectangle in tree order: each element box
/// plus each text run the box owns directly (its `run_range` minus the
/// children's subranges — the same bookkeeping as `transform_runs`). The
/// reverse scan in [`Layout::hit_hover`] then finds the deepest/latest
/// target first. `fixed` = inside a `position: fixed` subtree.
///
/// The root (body) box never becomes a target: `body` is an ancestor of
/// every element, so a cell of body would light every `:hover` rule on the
/// page — hovering the page background (or body's own text) matches
/// nothing (shortcut).
///
/// shortcut: tree order stands in for paint order (z-index/transforms are
/// not reflected), as in form hit-testing.
fn collect_hover(
    n: &BoxNode,
    runs: &[PositionedRun],
    fixed: bool,
    root_node: u32,
    out: &mut Vec<HoverTarget>,
) {
    if n.is_text {
        return; // a text leaf's runs are owned by its containing box
    }
    let fixed = fixed || n.bx.pos == Pos::Fixed;
    if let Some(node) = n.node {
        if node != root_node {
            out.push(HoverTarget {
                rect: (n.x, n.y, n.w, n.h),
                node,
                cursor: n.bx.cursor,
                fixed,
            });
        }
    }
    // this box's own run span = [run_range) minus in-flow children's
    // subranges (ranges are nested/disjoint, ascending in tree order)
    let (a, b) = (n.run_range.0 as usize, n.run_range.1 as usize);
    let mut cur = a;
    for ch in &n.children {
        let (ca, cb) = (ch.run_range.0 as usize, ch.run_range.1 as usize);
        if ca >= a && cb <= b && ca >= cur {
            push_run_targets(runs, cur, ca, n.bx.cursor, root_node, out);
            cur = cb;
        }
    }
    push_run_targets(runs, cur, b, n.bx.cursor, root_node, out);
    for ch in &n.children {
        collect_hover(ch, runs, fixed, root_node, out);
    }
}

/// Hover targets for `runs[lo..hi]` — one per run that carries a DOM node
/// (node-None runs are synthetic form-state leaves; their control box's
/// target already covers them). The run's own effective cursor wins, the
/// box's is the fallback for those unstamped leaves.
fn push_run_targets(
    runs: &[PositionedRun],
    lo: usize,
    hi: usize,
    box_cursor: Option<Cursor>,
    root_node: u32,
    out: &mut Vec<HoverTarget>,
) {
    for r in &runs[lo..hi] {
        if let Some(node) = r.node {
            if node == root_node {
                continue; // body's own text — same rule as the root box
            }
            out.push(HoverTarget {
                rect: (r.x, r.y, r.width, r.style.line_height()),
                node,
                cursor: r.cursor.or(box_cursor),
                fixed: r.fixed,
            });
        }
    }
}

impl Layout {
    /// Index of the topmost form control under viewport point (x, y),
    /// given the document is scrolled by `scroll` (fixed-position
    /// controls are viewport-relative already). The returned index
    /// addresses [`Layout::forms`].
    pub fn hit_test_form(&self, x: f32, y: f32, scroll: f32) -> Option<usize> {
        // later boxes paint over earlier ones → search back-to-front
        for (i, f) in self.forms.iter().enumerate().rev() {
            let fy = if f.fixed { y } else { y + scroll };
            let (fx, fyy, fw, fh) = f.rect;
            if x >= fx && x < fx + fw && fy >= fyy && fy < fyy + fh {
                return Some(i);
            }
        }
        None
    }

    /// Topmost hover target under viewport point (x, y), given the document
    /// is scrolled by `scroll` — same math as [`Self::hit_test_form`].
    /// Returns the hovered DOM element + its effective `cursor`.
    /// shortcut: targets are in tree order, not paint order (z-index and
    /// transforms aren't reflected), and clipped-away text stays hoverable
    /// — the same approximation as form hit-testing.
    pub fn hit_hover(&self, x: f32, y: f32, scroll: f32) -> Option<(u32, Option<Cursor>)> {
        for t in self.hover_targets.iter().rev() {
            let ty = if t.fixed { y } else { y + scroll };
            let (rx, ry, rw, rh) = t.rect;
            if x >= rx && x < rx + rw && ty >= ry && ty < ry + rh {
                return Some((t.node, t.cursor));
            }
        }
        None
    }

    /// Raw (never masked) value of a control by DOM node id — `""` when
    /// the node is not a known control.
    pub fn form_value(&self, node: u32) -> &str {
        self.form_values.get(&node).map(|s| s.as_str()).unwrap_or("")
    }

    /// Replace a control's value (the shell owns the editable string —
    /// it edits here and re-runs [`layout_in_forms`] to repaint).
    pub fn set_form_value(&mut self, node: u32, s: &str) {
        self.form_values.insert(node, s.to_string());
    }

    /// Snapshot of the interaction state to carry into the next
    /// [`layout_in_forms`] call — focus + live values survive re-layout
    /// because both are keyed by DOM node id, not box index.
    pub fn form_state(&self) -> FormState {
        FormState {
            focused: self.focused,
            values: self.form_values.clone(),
        }
    }
}

/// Post-layout walk (after [`position_all`], ranges final): thread each
/// box's clip window down the tree and intersect it into the runs and
/// underline rects its subtree emitted (`run_range`/`ul_range` are
/// start-inclusive/end-exclusive). Nested clips compose by intersection.
/// Runs are never removed (later phases index the vecs by range) — a
/// fully-outside run keeps a degenerate window (x0 ≥ x1) that main culls;
/// fully-outside underline rects collapse to zero size.
fn apply_run_clips(
    n: &BoxNode,
    clip: Option<(f32, f32, f32, f32)>,
    runs: &mut [PositionedRun],
    uls: &mut [UR],
) {
    if n.is_text {
        return; // text leaves hold no ranges (batched by their container)
    }
    // own clip:rect (positioned) clips the whole rendering incl. own text
    let clip = match (clip, clip_rect_window(n)) {
        (c, None) => c,
        (None, Some(w)) => Some(w),
        (Some(a), Some(w)) => Some(clip_intersect(a, w)),
    };
    // own overflow clips content (incl. own inline text), not own chrome
    let content_clip = if n.bx.clip {
        let bt = resolve_len(n.bx.border[0], n.w);
        let br = resolve_len(n.bx.border[1], n.w);
        let bb = resolve_len(n.bx.border[2], n.w);
        let bl = resolve_len(n.bx.border[3], n.w);
        let pad = (n.x + bl, n.y + bt, n.x + n.w - br, n.y + n.h - bb);
        Some(match clip {
            Some(c) => clip_intersect(c, pad),
            None => pad,
        })
    } else {
        clip
    };
    if let Some(w) = content_clip {
        let (a, b) = n.run_range;
        for r in &mut runs[a as usize..b as usize] {
            r.clip = Some(match r.clip {
                Some(c) => clip_intersect(c, w),
                None => w,
            });
        }
        let (a, b) = n.ul_range;
        for u in &mut uls[a as usize..b as usize] {
            // intersect in place — dropping would shift indices other
            // nodes' ranges point at; empty → zero size (drawn as nothing)
            let x0 = u.0.max(w.0);
            let y0 = u.1.max(w.1);
            let x1 = (u.0 + u.2).min(w.2);
            let y1 = (u.1 + u.3).min(w.3);
            if x1 > x0 && y1 > y0 {
                u.0 = x0;
                u.1 = y0;
                u.2 = x1 - x0;
                u.3 = y1 - y0;
            } else {
                u.2 = 0.0;
                u.3 = 0.0;
            }
        }
    }
    for ch in &n.children {
        apply_run_clips(ch, content_clip, runs, uls);
    }
}

/// Post-layout walk: every box with a background-color emits a filled rect;
/// every decoded `<img>` emits its pixels (parent-first, so children paint
/// over their parent's fill). A REPEATING background placeholder emits one
/// entry per tile of its recorded region (see [`BG_TILE_CAP`]). `fixed`
/// inherits down — a `position: fixed` subtree paints in viewport
/// coordinates.
///
/// `clip` = accumulated window (x0, y0, x1, y1) from ancestor `overflow`
/// boxes and `clip: rect()`, intersected into every pushed rect;
/// fully-outside rects are dropped. The element's own background/border/
/// shadow are NOT clipped by its OWN overflow (only by ancestors' clips and
/// its own `clip: rect()`, which per spec clips the whole rendering).
/// Windows arrive in PAINT space (mapped through [`Xf::map_win`] as each
/// transformed box establishes them), so a clip stays glued to the
/// rendering it bounds.
///
/// `m` = accumulated paint transform ([`Xf`]): a box's own `transform`
/// composes in before its rendering and is passed to its children, so the
/// whole subtree paints transformed while layout coords never change
/// (CSS transforms are visual-only). `z` = inherited stacking level,
/// stamped on every pushed rect/image for the stable sort in
/// `layout_in_forms`.
///
/// z-index model (simplified — `BoxStyle::z`): static boxes keep the
/// inherited level; a POSITIONED box with an integer `z-index` switches
/// its subtree to that level (`auto` keeps it → tree order, the
/// pre-z-index behavior). shortcut: integer-z boxes act as stacking-
/// context shortcuts — a nested integer z re-sorts against the whole
/// page instead of only within its parent's context — and z reorders
/// WITHIN each paint channel only (shadow < rect < image < text <
/// underline keep their fixed precedence), so a background never covers
/// another box's text; `render()`'s fixed pass likewise still draws last
/// regardless of z. Hit-testing (`hit_test_form`, the link walk in the
/// shell) still searches in tree order — z-index orders paint only.
fn collect_paint(
    n: &BoxNode,
    fixed: bool,
    m: Xf,
    z: i32,
    clip: Option<(f32, f32, f32, f32)>,
    // parent box's (y0, y1) — the sticky container edge recorded for
    // groups (shortcut: parent border box, not the spec's scrollport
    // containing block; the root's parent is the viewport)
    container: (f32, f32),
    out: &mut Vec<(f32, f32, f32, f32, u32, f32, bool)>,
    out_z: &mut Vec<i32>,
    imgs: &mut Vec<(f32, f32, f32, f32, std::rc::Rc<ImageData>, bool)>,
    img_z: &mut Vec<i32>,
    shadows: &mut Vec<ShadowRect>,
    sticky: &mut Vec<StickyGroup>,
) {
    if !n.is_text {
        let fixed = fixed || n.bx.pos == Pos::Fixed;
        // sticky box declaring `top`: open its group window BEFORE any of
        // this subtree's pushes (runs/uls come from the box's ranges) —
        // closed after the children recurse, so the spans cover exactly
        // this subtree's entries (the DFS pushes them contiguously)
        let sg = (n.bx.pos == Pos::Sticky && n.bx.inset[0].is_some()).then(|| {
            let i = sticky.len();
            sticky.push(StickyGroup {
                shadows: (shadows.len(), 0),
                rects: (out.len(), 0),
                imgs: (imgs.len(), 0),
                imgs_dec: (0, 0), // filled by main.rs's decode pass
                runs: (n.run_range.0 as usize, n.run_range.1 as usize),
                uls: (n.ul_range.0 as usize, n.ul_range.1 as usize),
                doc_y: n.y,
                doc_h: n.h,
                top: match n.bx.inset[0].unwrap() {
                    Len::Px(v) => v,
                    Len::Pct(p) => p / 100.0 * (container.1 - container.0),
                },
                container_y1: container.1,
                fixed,
            });
            i
        });
        // this box's transform joins the ancestor chain for its rendering
        // AND its children's; identity keeps every number below unchanged
        let m = match box_xf(n) {
            Some(t) => m.after(t),
            None => m,
        };
        // stacking level: static inherits, positioned takes its integer
        // z-index (`auto`/unset inherits → tree order — see model above)
        let z = match n.bx.pos {
            Pos::Static => z,
            _ => n.bx.z.unwrap_or(z),
        };
        let bt = resolve_len(n.bx.border[0], n.w);
        let br = resolve_len(n.bx.border[1], n.w);
        let bb = resolve_len(n.bx.border[2], n.w);
        let bl = resolve_len(n.bx.border[3], n.w);
        let has_b = bt > 0.0 || br > 0.0 || bb > 0.0 || bl > 0.0;
        // currentcolor: unspecified border color = the element's text color
        let bc = n.bx.bcolor.unwrap_or(n.style.color);
        let r = resolve_len(n.bx.radius, n.w).min(n.w * 0.5).min(n.h * 0.5);
        // a LIVE mask placeholder on this box (slot not de-slotted — the
        // budget cap may have dropped it) REPLACES the box's own
        // background: the bg-color rect below and the background-image
        // child are suppressed, and the mask paints in their place
        // (tinted with this box's bg color by `tint_masks`, cached in the
        // placeholder's slot). shortcut: the mask affects only the box's
        // OWN background — text and children are never masked; the border
        // (rounded or not) still paints over the mask, as the border is
        // chrome, not background.
        let masked = n.children.iter().any(|c| c.is_mask && c.img.is_some());
        let ring = has_b && r > 0.0 && n.bx.bg.is_some() && !masked;
        // own `clip: rect()` (positioned boxes) clips this box's WHOLE
        // rendering incl. its own background — composed with the inherited
        // one; the window is recorded in layout coords, so it is mapped
        // into paint space to stay glued to the transformed box
        let clip = match (clip, clip_rect_window(n).map(|w| m.map_win(w))) {
            (c, None) => c,
            (None, Some(w)) => Some(w),
            (Some(a), Some(w)) => Some(clip_intersect(a, w)),
        };
        // own overflow clips CONTENT (children + own image), not own chrome
        let content_clip = if n.bx.clip {
            let pad = (n.x + bl, n.y + bt, n.x + n.w - br, n.y + n.h - bb);
            Some(match clip {
                Some(c) => clip_intersect(c, m.map_win(pad)),
                None => m.map_win(pad),
            })
        } else {
            clip
        };
        // box-shadows: spread grows the rect, blur softens it; color
        // resolves to currentcolor when unspecified, alpha from the color
        for s in &n.bx.shadows {
            let sp = s.spread;
            let c = s.color.unwrap_or(n.style.color);
            let alpha = s.alpha as u32;
            // shortcut: culled against the un-blurred rect — blur may bleed
            // a few px past a clip edge (approximation, not spec-exact);
            // the shadow follows the box's transform but blur and alpha do
            // NOT scale (blur scaling needs a variable-radius falloff)
            let sx = n.x + s.dx - sp;
            let sy = n.y + s.dy - sp;
            let sw = (n.w + sp * 2.0).max(0.0);
            let sh = (n.h + sp * 2.0).max(0.0);
            let (sx, sy, sw, sh) = m.map_rect((sx, sy, sw, sh));
            if cull_rect((sx, sy, sw, sh), clip).is_none() {
                continue;
            }
            shadows.push(ShadowRect {
                x: sx,
                y: sy,
                w: sw,
                h: sh,
                radius: r * m.sx.abs(),
                blur: s.blur,
                color: (alpha << 24) | c,
                fixed,
            });
        }
        if let Some(c) = n.bx.bg.filter(|_| !masked) {
            let mut rects = vec![(n.x, n.y, n.w, n.h, c, r)];
            if ring {
                // rounded ring: fill the border-box with the border color,
                // then the background inset over it (inner radius ≈ r − border)
                rects[0].4 = bc;
                let inner = (r - bt.max(br).max(bb).max(bl)).max(0.0);
                rects.push((
                    n.x + bl,
                    n.y + bt,
                    (n.w - bl - br).max(0.0),
                    (n.h - bt - bb).max(0.0),
                    c,
                    inner,
                ));
            }
            for (x, y, w, h, color, rad) in rects {
                let (x, y, w, h) = m.map_rect((x, y, w, h));
                if let Some((x, y, w, h)) = cull_rect((x, y, w, h), clip) {
                    out.push((x, y, w, h, color, rad * m.sx.abs(), fixed));
                    out_z.push(z);
                }
            }
        }
        if has_b && !ring {
            // four side strips over the background (square corners on a
            // rounded backgroundless box — shortcut)
            let strips: [((f32, f32, f32, f32), bool); 4] = [
                ((n.x, n.y, n.w, bt), bt > 0.0),
                ((n.x, n.y + n.h - bb, n.w, bb), bb > 0.0),
                ((n.x, n.y + bt, bl, (n.h - bt - bb).max(0.0)), bl > 0.0),
                ((n.x + n.w - br, n.y + bt, br, (n.h - bt - bb).max(0.0)), br > 0.0),
            ];
            for (q, on) in strips {
                if !on {
                    continue;
                }
                let q = m.map_rect(q);
                if let Some(q) = cull_rect(q, clip) {
                    out.push((q.0, q.1, q.2, q.3, bc, 0.0, fixed));
                    out_z.push(z);
                }
            }
        }
        if let Some(ImgSlot::Ready(d)) = &n.img {
            if n.w > 0.0 && n.h > 0.0 {
                match (n.bx.bg_repeat, n.bg_tile_region) {
                    // a REPEATING background placeholder (declared repeat,
                    // region recorded by `place()`): one entry per tile
                    // over the region, each culled against the inherited
                    // clip window (ancestor overflow/clip from wave 1)
                    (Some(rep), Some(reg))
                        if (n.is_bg || n.is_mask)
                            && matches!(
                                rep,
                                BgRepeat::Repeat | BgRepeat::RepeatX | BgRepeat::RepeatY
                            ) =>
                    {
                        let (tw, th) = (n.w, n.h);
                        // candidate tile indices on each axis: tiles sit at
                        // origin + k·tile; the ±1 margin absorbs float
                        // slop, the clamp keeps the cast bounded (values
                        // are re-clamped to the cap below)
                        let (kx0, kx1) = if matches!(rep, BgRepeat::Repeat | BgRepeat::RepeatX)
                        {
                            (
                                (((reg.0 - n.x) / tw).floor() - 1.0).clamp(-1e5, 1e5) as i64,
                                (((reg.2 - n.x) / tw).ceil() + 1.0).clamp(-1e5, 1e5) as i64,
                            )
                        } else {
                            (0, 1) // repeat-y: a single column
                        };
                        let (ky0, ky1) = if matches!(rep, BgRepeat::Repeat | BgRepeat::RepeatY)
                        {
                            (
                                (((reg.1 - n.y) / th).floor() - 1.0).clamp(-1e5, 1e5) as i64,
                                (((reg.3 - n.y) / th).ceil() + 1.0).clamp(-1e5, 1e5) as i64,
                            )
                        } else {
                            (0, 1) // repeat-x: a single row
                        };
                        let cols = (kx1 - kx0).clamp(0, BG_TILE_CAP);
                        let rows = (ky1 - ky0).clamp(0, BG_TILE_CAP.div_euclid(cols.max(1)));
                        // tiles paint only inside the region (the padding
                        // box) ∩ the inherited clip window — edge tiles are
                        // cropped to it, same rect-culling the single-draw
                        // path uses at clip edges; both windows are mapped
                        // into paint space so the tiling travels with the
                        // transform (tile INDICES stay in layout coords)
                        let reg = m.map_win(reg);
                        let win = Some(match content_clip {
                            Some(c) => clip_intersect(c, reg),
                            None => reg,
                        });
                        for ky in ky0..ky0 + rows {
                            for kx in kx0..kx0 + cols {
                                let t = m.map_rect((n.x + kx as f32 * tw, n.y + ky as f32 * th, tw, th));
                                if let Some(q) = cull_rect(t, win) {
                                    imgs.push((q.0, q.1, q.2, q.3, d.clone(), fixed));
                                    img_z.push(z);
                                }
                            }
                        }
                    }
                    _ => {
                        // single draw at the object-fit content rect (the
                        // box itself under the initial `fill`), cropped to
                        // the fit window when cover/none overflow it
                        let src = n.img_fit.unwrap_or((n.x, n.y, n.w, n.h));
                        let q = m.map_rect(src);
                        let win = match (content_clip, n.img_clip.map(|c| m.map_win(c))) {
                            (None, w) => w,
                            (Some(a), Some(b)) => Some(clip_intersect(a, b)),
                            (Some(a), None) => Some(a),
                        };
                        if let Some(q) = cull_rect(q, win) {
                            imgs.push((q.0, q.1, q.2, q.3, d.clone(), fixed));
                            img_z.push(z);
                        }
                    }
                }
            }
        }
        for ch in &n.children {
            // mask wins over the box's own background image: the bg
            // placeholder never reaches its draw branch while masked
            if masked && ch.is_bg {
                continue;
            }
            collect_paint(
                ch,
                fixed,
                m,
                z,
                content_clip,
                (n.y, n.y + n.h),
                out,
                out_z,
                imgs,
                img_z,
                shadows,
                sticky,
            );
        }
        // close the sticky group's window (own entries + every descendant)
        if let Some(i) = sg {
            sticky[i].shadows.1 = shadows.len();
            sticky[i].rects.1 = out.len();
            sticky[i].imgs.1 = imgs.len();
        }
    }
}

/// Symbolic length → px against the container width.
fn resolve_len(l: Len, avail: f32) -> f32 {
    match l {
        Len::Px(p) => p,
        Len::Pct(p) => p / 100.0 * avail,
    }
}

/// Paint-time transform: `p' = (p.x·sx + tx, p.y·sy + ty)` — a scale about
/// the box's transform-origin plus a translate, composed down the tree so
/// a transformed box moves/scales its WHOLE subtree's painting. Diagonal
/// only: `rotate()`/`skew()` never reach here (css.rs `parse_transform`
/// drops them — the rasterizer draws axis-aligned rects + horizontal glyph
/// runs, so faking rotated pixels would lie), hence every mapped rect
/// stays axis-aligned and the existing draw code handles it.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Xf {
    sx: f32,
    sy: f32,
    tx: f32,
    ty: f32,
}

impl Xf {
    const IDENT: Xf = Xf { sx: 1.0, sy: 1.0, tx: 0.0, ty: 0.0 };

    fn is_ident(&self) -> bool {
        *self == Xf::IDENT
    }

    /// `self` applied AFTER `inner` — the paint position of a point whose
    /// local position first goes through `inner` (a child box's own
    /// transform) and then through `self` (its ancestors').
    fn after(self, inner: Xf) -> Xf {
        Xf {
            sx: self.sx * inner.sx,
            sy: self.sy * inner.sy,
            tx: self.tx * inner.sx + inner.tx,
            ty: self.ty * inner.sy + inner.ty,
        }
    }

    fn map_pt(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.sx + self.tx, y * self.sy + self.ty)
    }

    /// Map a paint rect (x, y, w, h) — negative scales flip it but keep it
    /// axis-aligned. Identity returns the input unchanged (byte-identical
    /// paint on pages without `transform`).
    fn map_rect(&self, r: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        if self.is_ident() {
            return r;
        }
        let (a, b) = self.map_pt(r.0, r.1);
        let (c, d) = self.map_pt(r.0 + r.2, r.1 + r.3);
        (a.min(c), b.min(d), (a - c).abs(), (b - d).abs())
    }

    /// Map a clip window's corners (kept as x0, y0, x1, y1, normalized) —
    /// clips are recorded in layout coords, so a transformed box's
    /// `overflow`/`clip: rect()` window travels WITH its rendering.
    fn map_win(&self, w: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        if self.is_ident() {
            return w;
        }
        let (a, b) = self.map_pt(w.0, w.1);
        let (c, d) = self.map_pt(w.2, w.3);
        (a.min(c), b.min(d), a.max(c), b.max(d))
    }
}

/// The box's own [`css::Transform`] as an [`Xf`] about its transform
/// origin — CSS default `center` (shortcut: `transform-origin` isn't
/// parsed; centering is what `scale()`/`translate()` patterns expect).
/// `%` translate resolves against the box's own w/h here (layout coords);
/// None = no transform.
fn box_xf(n: &BoxNode) -> Option<Xf> {
    let t = n.bx.transform?;
    let tx = resolve_len(t.tx, n.w);
    let ty = resolve_len(t.ty, n.h);
    // p' = (p − c)·s + c + d  ⇔  p·s + (c(1 − s) + d), c = box center
    Some(Xf {
        sx: t.sx,
        sy: t.sy,
        tx: (n.x + n.w * 0.5) * (1.0 - t.sx) + tx,
        ty: (n.y + n.h * 0.5) * (1.0 - t.sy) + ty,
    })
}

/// Stable-sort `v` by the parallel stacking-level `keys` (both built in
/// the same tree walk). Stability keeps TREE order within a level — the
/// CSS tie-break — and an all-equal key list (no `z-index` anywhere)
/// permutes to itself, so pages without the property are untouched.
///
/// Returns `Some(map)` with `map[orig] = new index` when it actually
/// permuted (sticky groups remap their recorded spans through it — see
/// [`remap_sticky_spans`]), `None` on the identity fast path.
fn zsort<T>(v: &mut Vec<T>, keys: &[i32]) -> Option<Vec<usize>> {
    debug_assert_eq!(v.len(), keys.len(), "zsort: keys must match entries");
    if keys.windows(2).all(|w| w[0] == w[1]) {
        return None; // one level (incl. the empty/single-entry case) = tree order
    }
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by_key(|&i| keys[i]); // stable: ties keep tree order
    let mut map = vec![0usize; idx.len()];
    for (new, &orig) in idx.iter().enumerate() {
        map[orig] = new;
    }
    let orig = std::mem::take(v);
    let mut slots: Vec<Option<T>> = orig.into_iter().map(Some).collect();
    for i in idx {
        v.push(slots[i].take().unwrap());
    }
    Some(map)
}

/// Remap one channel's sticky-group spans through [`zsort`]'s index map
/// so they still cover the same subtree after the channel was re-sorted.
/// A subtree that kept ONE stacking level lands in one contiguous run
/// (stable sort) → exact remap. A nested integer `z-index` inside the
/// sticky box splits the subtree across levels → the run can't be
/// represented as a span, so the group is DISABLED: its clamp range is
/// forced to 0, which pins nothing in ANY channel (safe degradation —
/// shortcut: sticky under a splitting nested z never pins).
fn remap_sticky_spans(
    sticky: &mut [StickyGroup],
    span: impl Fn(&mut StickyGroup) -> &mut (usize, usize),
    map: &[usize],
) {
    for g in sticky.iter_mut() {
        let (a, b) = *span(g);
        let ok = a < b
            && b <= map.len()
            && (a..b).all(|i| map[i] == map[a] + (i - a));
        if !ok {
            if a < b {
                g.container_y1 = g.doc_y + g.doc_h; // room 0 → no pin
            }
            continue;
        }
        *span(g) = (map[a], map[a] + (b - a));
    }
}

/// Post-layout walk: apply each box's visual transform ([`Xf`]) to the
/// text runs and underline rects its subtree emitted, and stamp every run
/// with its stacking level (for [`zsort`] in `layout_in_forms`). Layout
/// coords never move — CSS transforms are visual-only — so this is the
/// only place runs shift.
///
/// A box maps only the runs of its OWN span (its range minus its
/// in-flow children's subranges — disjoint and in order; an absolute
/// child's late range sits outside the parent's and is mapped when the
/// walk reaches that child), so nested transforms compose exactly once.
///
/// shortcut: a run's GLYPHS are not scaled or rotated — only the run's
/// anchor moves (translate is exact; `scale()` recenters text without
/// resizing it — scaled glyphs need an affine raster path this engine
/// doesn't have). A run's clip window moves through the same matrix, so
/// a clip set above a transformed ancestor shifts with it (spec keeps it
/// fixed) — visible only when authors combine overflow clipping with a
/// transform between the clipper and the text. `underline_rects` are
/// mapped but not z-sorted (that channel paints last within a pass
/// anyway — caret/underline over text is the existing look).
fn transform_runs(
    n: &BoxNode,
    m: Xf,
    z: i32,
    runs: &mut [PositionedRun],
    uls: &mut [UR],
    run_z: &mut [i32],
) {
    if n.is_text {
        return;
    }
    let m = match box_xf(n) {
        Some(t) => m.after(t),
        None => m,
    };
    let z = match n.bx.pos {
        Pos::Static => z,
        _ => n.bx.z.unwrap_or(z),
    };
    // own run span = [run_range) minus in-flow children's subranges
    let (a, b) = (n.run_range.0 as usize, n.run_range.1 as usize);
    let mut cur = a;
    for ch in &n.children {
        let (ca, cb) = (ch.run_range.0 as usize, ch.run_range.1 as usize);
        if ca >= a && cb <= b && ca >= cur {
            map_runs(m, z, cur, ca, runs, run_z);
            cur = cb;
        }
    }
    map_runs(m, z, cur, b, runs, run_z);
    // the underline channel has its own parallel ranges
    let (ua, ub) = (n.ul_range.0 as usize, n.ul_range.1 as usize);
    let mut ucur = ua;
    for ch in &n.children {
        let (ca, cb) = (ch.ul_range.0 as usize, ch.ul_range.1 as usize);
        if ca >= ua && cb <= ub && ca >= ucur {
            map_uls(m, ucur, ca, uls);
            ucur = cb;
        }
    }
    map_uls(m, ucur, ub, uls);
    for ch in &n.children {
        transform_runs(ch, m, z, runs, uls, run_z);
    }
}

/// Map one run span through `m` and stamp its stacking level.
fn map_runs(m: Xf, z: i32, lo: usize, hi: usize, runs: &mut [PositionedRun], run_z: &mut [i32]) {
    debug_assert!(lo <= hi);
    if !m.is_ident() {
        for r in &mut runs[lo..hi] {
            r.x = r.x * m.sx + m.tx;
            r.y = r.y * m.sy + m.ty;
            r.clip = r.clip.map(|c| m.map_win(c));
        }
    }
    for k in &mut run_z[lo..hi] {
        *k = z;
    }
}

/// Map one underline-rect span through `m` (no z key — see
/// [`transform_runs`]).
fn map_uls(m: Xf, lo: usize, hi: usize, uls: &mut [UR]) {
    debug_assert!(lo <= hi);
    if m.is_ident() {
        return;
    }
    for u in &mut uls[lo..hi] {
        let (x, y, w, h, color, fixed) = *u;
        let (x, y, w, h) = m.map_rect((x, y, w, h));
        *u = (x, y, w, h, color, fixed);
    }
}

/// Clip window (x0, y0, x1, y1) ∩ another window — nested clips compose.
fn clip_intersect(
    a: (f32, f32, f32, f32),
    b: (f32, f32, f32, f32),
) -> (f32, f32, f32, f32) {
    (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3))
}

/// Intersect a paint rect (x, y, w, h) with an optional clip window; no
/// window → unchanged, fully-outside → None (drop), straddling → the
/// clipped remainder.
fn cull_rect(
    r: (f32, f32, f32, f32),
    clip: Option<(f32, f32, f32, f32)>,
) -> Option<(f32, f32, f32, f32)> {
    let Some(c) = clip else {
        return Some(r);
    };
    let x0 = r.0.max(c.0);
    let y0 = r.1.max(c.1);
    let x1 = (r.0 + r.2).min(c.2);
    let y1 = (r.1 + r.3).min(c.3);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// `clip: rect(top, right, bottom, left)` on a POSITIONED box → the window
/// (x0, y0, x1, y1) it carves out of the border box. Offsets are from the
/// border-box top-left (all four), so `rect(1px,1px,1px,1px)` is a zero-area
/// window — the classic skip-link hide. Static boxes ignore `clip` (spec).
/// `%` offsets resolve against the border box (spec says lengths only;
/// lenient parse).
fn clip_rect_window(n: &BoxNode) -> Option<(f32, f32, f32, f32)> {
    let c = n.bx.clip_rect?;
    if n.bx.pos == Pos::Static {
        return None;
    }
    let x0 = n.x + resolve_len(c[3], n.w);
    let y0 = n.y + resolve_len(c[0], n.h);
    let x1 = n.x + resolve_len(c[1], n.w);
    let y1 = n.y + resolve_len(c[2], n.h);
    Some((x0, y0, x1, y1))
}

fn leaf_atom(n: &BoxNode) -> Atom {
    Atom {
        text: n.text.clone(),
        style: n.style,
        hard_break: n.hard_break,
        link: n.link.clone(),
        node: n.node,
        cursor: n.bx.cursor,
    }
}

/// Shift an already-laid subtree by (dx, dy) — used by flex cross-axis align
/// and by the paragraph-gap insertion after the child was placed.
fn shift_tree(n: &mut BoxNode, dx: f32, dy: f32) {
    n.x += dx;
    n.y += dy;
    for c in &mut n.children {
        shift_tree(c, dx, dy);
    }
}

/// Containing block (x, y, w, h) — the nearest positioned ancestor's padding
/// box, or the viewport for `position: fixed`.
type CB = (f32, f32, f32, f32);

/// Place one node. `avail` = containing block's content width.
/// Returns the outer vertical advance (margin-top + border-box height +
/// margin-bottom) so block flow can stack siblings without overlap.
/// `flt` = ancestor floats the box's TEXT flow avoids — only `block()`
/// consumes it; flex/grid/absolute callers pass `&[]` (floats never
/// affect those, per spec).
fn place(
    n: &mut BoxNode,
    x: f32,
    y: f32,
    avail: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
    runs: &mut Vec<PositionedRun>,
    uls: &mut Vec<UR>,
    flt: &[Flt],
) -> f32 {
    // every run/underline this subtree emits lands in [run0..]/[ul0..] —
    // stored on the node so phase-2 repositioning can shift exactly that
    // range (nested shifts compose; appends never move existing entries)
    let (run0, ul0) = (runs.len(), uls.len());
    n.x = x;
    n.y = y;
    debug_assert!(!n.is_text, "text leaves are batched by their container");
    // margins (symbolic % resolves against the container width)
    let ml = resolve_len(n.bx.margin[3], avail);
    let mr = resolve_len(n.bx.margin[1], avail);
    let mt = resolve_len(n.bx.margin[0], avail);
    let mb = resolve_len(n.bx.margin[2], avail);
    n.x += ml;
    n.y += mt;
    // resolve border-box width
    let mut w = (avail - ml - mr).max(0.0);
    if let Some(l) = n.bx.width {
        w = resolve_len(l, avail);
    }
    if let Some(m) = n.bx.max_width {
        w = w.min(resolve_len(m, avail));
    }
    // aspect-ratio with one declared axis derives the other. A declared
    // height + ratio sets the width here (an auto block width would
    // otherwise fill the container); a declared width derives the height
    // after content placement below.
    if n.bx.width.is_none() {
        if let (Some(h), Some(r)) = (n.bx.height, n.bx.aspect_ratio) {
            w = resolve_len(h, avail) * r;
            if let Some(m) = n.bx.max_width {
                w = w.min(resolve_len(m, avail));
            }
        }
    }
    // replaced element with known intrinsic size: width unless overridden
    // (a declared height + aspect-ratio above already resolved the width)
    if n.img.is_some()
        && n.bx.width.is_none()
        && (n.bx.height.is_none() || n.bx.aspect_ratio.is_none())
    {
        if let Some((iw, _)) = n.img.as_ref().and_then(ImgSlot::dims) {
            w = (iw as f32).min(avail);
        }
    }
    // border box = content inset by borders + padding (shortcut: default
    // model is border-box — an explicit `width` includes both, matching
    // most modern sites). shortcut: %-padding/border resolves against the
    // box's own width below; with `box-sizing: content-box` that is the
    // declared CONTENT width, not the border box a %-value would resolve
    // against in CSS — cyclic (pads feed the width they resolve inside);
    // px padding (the overwhelmingly common case) is exact
    let pl = resolve_len(n.bx.pad[3], w);
    let pr = resolve_len(n.bx.pad[1], w);
    let pt = resolve_len(n.bx.pad[0], w);
    let pb = resolve_len(n.bx.pad[2], w);
    let bl = resolve_len(n.bx.border[3], w);
    let br = resolve_len(n.bx.border[1], w);
    let bt = resolve_len(n.bx.border[0], w);
    let bb = resolve_len(n.bx.border[2], w);
    if n.bx.content_box && n.bx.width.is_some() {
        // content-box: the declared width measures content only — grow the
        // border box by padding + border so the content width stays exactly
        // the declared value (max-width above already clamped that same
        // content width, matching content-box CSS; the border box may
        // exceed max-width by the pads, per spec)
        w += pl + pr + bl + br;
    }
    // auto margins: both horizontal auto → center; only-left-auto → the
    // auto margin absorbs free space on the left, pushing the box right
    // (right-auto-only needs no branch — the box already hugs the left).
    // after the content-box growth above, so centering uses the final
    // border-box width
    if n.bx.margin_x_auto {
        n.x = x + (avail - w).max(0.0) * 0.5;
    } else if n.bx.margin_l_auto {
        n.x = x + (avail - w).max(0.0);
    }
    let (bx0, by0) = (n.x, n.y);
    n.x += pl + bl;
    n.y += pt + bt;
    n.w = (w - pl - pr - bl - br).max(0.0);
    // (background-image placeholders are placed at the END of place(),
    // once the used padding box — incl. content height + min-height — is
    // known, so %-sizes/%-positions resolve against the final box)
    if n.img.is_some() {
        // replaced element: no children — height from declared height,
        // else the declared aspect ratio, else the intrinsic ratio of the
        // content width (no size known: 0 → invisible)
        n.h = match n.bx.height {
            Some(l) => resolve_len(l, avail),
            None => match n.bx.aspect_ratio {
                Some(r) => n.w / r,
                None => match n.img.as_ref().and_then(ImgSlot::dims) {
                    Some((iw, ih)) => n.w * ih as f32 / (iw as f32).max(1.0),
                    None => 0.0,
                },
            },
        };
    } else {
        match (n.bx.display, n.bx.dir) {
            (Display::Flex, Dir::Row) => flex_row(n, measure, runs, uls),
            (Display::Flex, Dir::Column) => flex_col(n, measure, runs, uls),
            (Display::Grid, _) => grid(n, measure, runs, uls),
            _ => block(n, measure, runs, uls, flt), // Block / Auto(block tag)
        }
        // aspect-ratio with auto height: the box keeps the ratio from its
        // content width and its content is laid out inside (content taller
        // than the ratio box overflows, per spec — no min-height rescuing
        // here beyond the explicit `min-height` floor below). A declared
        // height + ratio pins that height (blocks otherwise ignore `height`).
        // shortcut: flex/grid cross-axis stretch still decides its own axis
        // (the ratio only feeds this place() derivation).
        if let Some(r) = n.bx.aspect_ratio {
            n.h = match n.bx.height {
                Some(l) => resolve_len(l, avail),
                None => n.w / r,
            };
        }
    }
    // min-height: floor the content height. A flex column with vertical
    // `justify` pushes its stack into the freed space (hero uses
    // `min-h-[560px] justify-end` so the NEXT outline floats above the pill).
    // shortcut: `%` min-height resolves against width, not the container
    // height (real pages use px here).
    if let Some(mh) = n.bx.min_height {
        let min_px = resolve_len(mh, avail);
        if n.h < min_px {
            let dy = match (n.bx.display, n.bx.dir, n.bx.justify) {
                (Display::Flex, Dir::Column, Justify::Center) => (min_px - n.h) * 0.5,
                (Display::Flex, Dir::Column, Justify::End) => min_px - n.h,
                _ => 0.0,
            };
            if dy != 0.0 {
                for ch in &mut n.children {
                    shift_tree(ch, 0.0, dy);
                }
                for r in &mut runs[run0..] {
                    r.y += dy;
                }
                for u in &mut uls[ul0..] {
                    u.1 += dy;
                }
            }
            n.h = min_px;
        }
    }
    // restore border-box coords; height includes vertical padding + borders
    n.h += pt + pb + bt + bb;
    n.x = bx0;
    n.y = by0;
    n.w = w;
    // object-fit content rect for a content <img>: `fill` (or no slot) keeps
    // the legacy box draw byte-identically, every other mode computes the
    // letterbox/crop inside the padding box and (for cover/none) the visible
    // window decode trims to.
    if n.img.is_some() && !n.is_bg && !n.is_mask && n.bx.obj_fit != ObjFit::Fill {
        if let Some((iw, ih)) = n.img.as_ref().and_then(ImgSlot::dims) {
            let fit_box = (
                n.x + bl,
                n.y + bt,
                (n.w - bl - br).max(0.0),
                (n.h - bt - bb).max(0.0),
            );
            let (rect, clip) =
                fit_content_rect(n.bx.obj_fit, n.bx.obj_pos, fit_box, iw as f32, ih as f32);
            n.img_fit = Some(rect);
            n.img_clip = clip;
        }
    }
    // background-image placeholders: sized per `background-size` and
    // anchored per `background-position` inside this box's padding box —
    // placed AFTER content layout so %-sizes/%-positions resolve against
    // the USED padding box (incl. min-height). Out of flow (block/flex/
    // grid skipped them above), so they take no space. An unprobed slot
    // (fetch failed / unsupported format / over budget) stays 0×0 and
    // never paints. shortcut: the tiling region is the padding box (CSS
    // default background-clip is the border box; the border sliver of a
    // repeating pattern is a few px of texture).
    let (pbx, pby) = (n.x + bl, n.y + bt);
    let (pbw, pbh) = (n.w - bl - br, n.h - bt - bb);
    for ch in &mut n.children {
        // both placeholders are sized/positioned by the same math against
        // this box's padding box (mask geometry rides `ch.bx.bg_*` too —
        // mask_placeholder copied the mask longhands there)
        if !ch.is_bg && !ch.is_mask {
            continue;
        }
        let Some((iw, ih)) = ch.img.as_ref().and_then(ImgSlot::dims) else {
            continue;
        };
        let (iw, ih) = (iw as f32, ih as f32);
        // drawn tile size per `background-size` — % resolves against the
        // padding box; an `auto` axis keeps the intrinsic size or, next to
        // an explicit axis, the intrinsic aspect ratio
        let (tw, th) = match ch.bx.bg_size {
            BgSize::Auto => (iw, ih),
            BgSize::Cover => {
                let s = (pbw / iw.max(1.0)).max(pbh / ih.max(1.0));
                (iw * s, ih * s)
            }
            BgSize::Contain => {
                let s = (pbw / iw.max(1.0)).min(pbh / ih.max(1.0));
                (iw * s, ih * s)
            }
            BgSize::Wh(wopt, hopt) => match (wopt, hopt) {
                (None, None) => (iw, ih),
                (Some(l), None) => {
                    let bw = resolve_len(l, pbw);
                    (bw, bw * ih / iw.max(1.0))
                }
                (None, Some(l)) => {
                    let bh = resolve_len(l, pbh);
                    (bh * iw / ih.max(1.0), bh)
                }
                (Some(lw), Some(lh)) => (resolve_len(lw, pbw), resolve_len(lh, pbh)),
            },
        };
        // position per `background-position`: % = (padding box − tile) × p
        // (0% flush left/top, 100% flush right/bottom); lengths offset from
        // the padding-box edge
        let ox = match ch.bx.bg_pos[0] {
            BgPos::Pct(p) => (pbw - tw) * p / 100.0,
            BgPos::Px(v) => v,
        };
        let oy = match ch.bx.bg_pos[1] {
            BgPos::Pct(p) => (pbh - th) * p / 100.0,
            BgPos::Px(v) => v,
        };
        ch.x = pbx + ox;
        ch.y = pby + oy;
        ch.w = tw;
        ch.h = th;
        // a DECLARED repeat records the tiling region (`collect_paint`
        // emits one entry per tile over it); undeclared / no-repeat stays
        // a single draw (see the BgRepeat doc for the initial-value
        // shortcut)
        ch.bg_tile_region = ch
            .bx
            .bg_repeat
            .filter(|r| *r != BgRepeat::NoRepeat)
            .map(|_| (pbx, pby, pbx + pbw, pby + pbh));
    }
    n.run_range = (run0 as u32, runs.len() as u32);
    n.ul_range = (ul0 as u32, uls.len() as u32);
    mt + n.h + mb
}

/// Phase-2 walk — resolve non-static boxes once every in-flow position is
/// final (parent-first so nested relative/absolute shifts compose).
/// `cb` = nearest positioned ancestor's padding box (abs), `flow` = parent's
/// content box (relative % insets), `vcb` = viewport (fixed), `fixed` =
/// inside a `position: fixed` subtree (paints without scroll offset).
fn position_all(
    n: &mut BoxNode,
    cb: CB,
    flow: CB,
    vcb: CB,
    fixed: bool,
    measure: &dyn Fn(&str, &Style) -> f32,
    runs: &mut Vec<PositionedRun>,
    uls: &mut Vec<UR>,
) {
    let pos = n.bx.pos;
    let fixed = fixed || pos == Pos::Fixed;
    let inset = n.bx.inset; // [Option<Len>; 4] is Copy — avoid borrow conflicts
    match pos {
        // in flow already (also the root, laid by layout_in)
        Pos::Static => {}
        // sticky: in flow EXACTLY like static — its threshold is scroll-
        // dependent and never offsets layout (a declared `top: 40px`
        // shifts nothing here, unlike relative; paint pins it later —
        // see StickyGroup). Out-of-flow matches below never see it, so it
        // keeps its flow space (that's why it exists).
        Pos::Sticky => {}
        // stays in flow, just shifts — dx = left (wins) else −right
        Pos::Relative => {
            let dx = match (inset[3], inset[1]) {
                (Some(l), _) => resolve_len(l, flow.2),
                (_, Some(r)) => -resolve_len(r, flow.2),
                _ => 0.0,
            };
            let dy = match (inset[0], inset[2]) {
                (Some(t), _) => resolve_len(t, flow.3),
                (_, Some(b)) => -resolve_len(b, flow.3),
                _ => 0.0,
            };
            if dx != 0.0 || dy != 0.0 {
                shift_tree(n, dx, dy);
                let (a, b) = n.run_range;
                for r in &mut runs[a as usize..b as usize] {
                    r.x += dx;
                    r.y += dy;
                }
                let (a, b) = n.ul_range;
                for u in &mut uls[a as usize..b as usize] {
                    u.0 += dx;
                    u.1 += dy;
                }
            }
        }
        // left flow during place() — lay it out now against its containing
        // block, then shift to satisfy the insets
        Pos::Absolute | Pos::Fixed => {
            let c = if pos == Pos::Fixed { vcb } else { cb };
            // pre-size: both horizontal insets → stretch; else shrink-to-fit
            // (max-content clamped to the CB — shortcut: no min-content floor)
            // (img keeps its intrinsic width from place())
            if n.bx.width.is_none() && n.img.is_none() {
                // pre-size in border-box terms, then restate as the
                // content width a content-box declaration carries
                // (place() grows it back — see as_content_decl)
                n.bx.width = match (inset[3], inset[1]) {
                    (Some(l), Some(r)) => Some(Len::Px(as_content_decl(
                        (c.2 - resolve_len(l, c.2) - resolve_len(r, c.2)).max(0.0),
                        &n.bx,
                        c.2,
                    ))),
                    _ => Some(Len::Px(as_content_decl(
                        natural_w(n, c.2, measure).min(c.2),
                        &n.bx,
                        c.2,
                    ))),
                };
            }
            // provisional = CB origin (shortcut: real static position was
            // skipped by the flow); insets resolve % against the CB axis.
            // Negative insets are honored verbatim — `top:-9999px` lays the
            // box off-canvas (nothing clamps it back), the classic
            // off-screen skip-link pattern. `overflow`/`clip` are parsed
            // (BoxStyle.clip / clip_rect): overflow clips descendants to the
            // padding box at paint, `clip: rect()` (positioned boxes) clips
            // the whole rendering — so a 1px clip-rect + overflow:hidden
            // skip-link that declares NO insets stays at the CB origin but
            // paints as nothing.
            // out of flow: floats don't apply (spec)
            place(n, c.0, c.1, c.2, measure, runs, uls, &[]);
            let ml = resolve_len(n.bx.margin[3], c.2);
            let mr = resolve_len(n.bx.margin[1], c.2);
            let mt = resolve_len(n.bx.margin[0], c.2);
            let mb = resolve_len(n.bx.margin[2], c.2);
            // over-constrained → top/left wins (ltr)
            let dx = match (inset[3], inset[1]) {
                (Some(l), _) => c.0 + resolve_len(l, c.2) + ml - n.x,
                (_, Some(r)) => c.0 + c.2 - resolve_len(r, c.2) - mr - n.w - n.x,
                _ => 0.0,
            };
            let dy = match (inset[0], inset[2]) {
                (Some(t), _) => c.1 + resolve_len(t, c.3) + mt - n.y,
                (_, Some(b)) => c.1 + c.3 - resolve_len(b, c.3) - mb - n.h - n.y,
                _ => 0.0,
            };
            if dx != 0.0 || dy != 0.0 {
                shift_tree(n, dx, dy);
                let (a, b) = n.run_range;
                for r in &mut runs[a as usize..b as usize] {
                    r.x += dx;
                    r.y += dy;
                }
                let (a, b) = n.ul_range;
                for u in &mut uls[a as usize..b as usize] {
                    u.0 += dx;
                    u.1 += dy;
                }
            }
            if fixed {
                let (a, b) = n.run_range;
                for r in &mut runs[a as usize..b as usize] {
                    r.fixed = true;
                }
                let (a, b) = n.ul_range;
                for u in &mut uls[a as usize..b as usize] {
                    u.5 = true;
                }
            }
        }
    }
    // CB for descendants: positioned boxes expose their padding box; everyone
    // else forwards theirs. The flow box (relative %) = own content box.
    let kid_cb = if pos == Pos::Static {
        cb
    } else {
        let bl = resolve_len(n.bx.border[3], n.w);
        let br = resolve_len(n.bx.border[1], n.w);
        let bt = resolve_len(n.bx.border[0], n.w);
        let bb = resolve_len(n.bx.border[2], n.w);
        (n.x + bl, n.y + bt, (n.w - bl - br).max(0.0), (n.h - bt - bb).max(0.0))
    };
    let pl = resolve_len(n.bx.pad[3], n.w);
    let pr = resolve_len(n.bx.pad[1], n.w);
    let pt = resolve_len(n.bx.pad[0], n.w);
    let pb = resolve_len(n.bx.pad[2], n.w);
    let kid_flow = (
        kid_cb.0 + pl,
        kid_cb.1 + pt,
        (kid_cb.2 - pl - pr).max(0.0),
        (kid_cb.3 - pt - pb).max(0.0),
    );
    for ch in &mut n.children {
        position_all(ch, kid_cb, kid_flow, vcb, fixed, measure, runs, uls);
    }
}

type UR = (f32, f32, f32, f32, u32, bool); // x, y, w, h, color, fixed

fn push_runs(mut src: Vec<PositionedRun>, dx: f32, dy: f32, dst: &mut Vec<PositionedRun>) {
    for r in &mut src {
        r.x += dx;
        r.y += dy;
    }
    dst.append(&mut src);
}

fn push_uls(mut src: Vec<UR>, dx: f32, dy: f32, dst: &mut Vec<UR>) {
    for u in &mut src {
        u.0 += dx;
        u.1 += dy;
    }
    dst.append(&mut src);
}

/// The 1–2 items of a flex/grid row: consecutive text leaves merge into one
/// anonymous item (matching the spec's anonymous flex item).
#[derive(Clone, Copy)]
enum Item {
    Text(usize, usize), // range into children
    Grp(usize),
}

fn items_of(children: &[BoxNode]) -> Vec<Item> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < children.len() {
        if children[i].is_text {
            let start = i;
            while i < children.len() && children[i].is_text {
                i += 1;
            }
            // whitespace-only runs are not items (spec: anonymous item
            // creation skips whitespace) — they used to eat a grid track
            if children[start..i].iter().all(|c| c.text.trim().is_empty()) {
                continue;
            }
            out.push(Item::Text(start, i));
        } else if children[i].is_bg
            || children[i].is_mask
            || matches!(children[i].bx.pos, Pos::Absolute | Pos::Fixed)
        {
            i += 1; // background/mask placeholder / out of flow: not a layout item
        } else {
            out.push(Item::Grp(i));
            i += 1;
        }
    }
    out
}

/// Restate a border-box width (padding+border included) as the content
/// width a `box-sizing: content-box` declaration must carry — `place()`
/// grows a declared width by padding+border when `content_box` is set, so
/// values the layout pre-computes in border-box terms (absolute-position
/// pre-sizing, the flex %-slot conversion) subtract that back out here.
/// px padding/border is exact; shortcut: %-padding resolves against
/// `base` here but against the (smaller) content width inside `place()`.
fn as_content_decl(w: f32, bx: &BoxStyle, base: f32) -> f32 {
    if !bx.content_box {
        return w;
    }
    let pb = resolve_len(bx.pad[1], base)
        + resolve_len(bx.pad[3], base)
        + resolve_len(bx.border[1], base)
        + resolve_len(bx.border[3], base);
    (w - pb).max(0.0)
}

/// max-content (unwrapped) width of a node, including horizontal padding.
fn natural_w(n: &BoxNode, avail: f32, measure: &dyn Fn(&str, &Style) -> f32) -> f32 {
    let content = natural_w_inner(n, avail, measure);
    if n.is_text {
        return content;
    }
    content
        + resolve_len(n.bx.pad[3], avail)
        + resolve_len(n.bx.pad[1], avail)
        + resolve_len(n.bx.border[3], avail)
        + resolve_len(n.bx.border[1], avail)
}

fn natural_w_inner(n: &BoxNode, avail: f32, measure: &dyn Fn(&str, &Style) -> f32) -> f32 {
    if n.is_text {
        return if n.hard_break {
            0.0
        } else {
            // + tracking: shrink-to-fit widths count letter/word spacing
            // (same spacing_extra rule as line layout and draw_text)
            measure(&n.text, &n.style) + spacing_extra(&n.text, &n.style)
        };
    }
    if let Some(l) = n.bx.width {
        if !matches!(l, Len::Pct(_)) {
            return resolve_len(l, avail);
        }
        // shortcut: a % width is cyclic in max-content sizing (the box
        // would resolve it against the shrink-to-fit width it feeds) —
        // measured intrinsically (auto), like real engines size
        // max-content; place() still resolves the % against the
        // containing block when laying out
    }
    if let Some((iw, _)) = n.img.as_ref().and_then(ImgSlot::dims) {
        return iw as f32; // intrinsic (padding added by the wrapper)
    }
    match (n.bx.display, n.bx.dir) {
        (Display::Flex, Dir::Row) => {
            let items = items_of(&n.children);
            let k = items.len();
            let mut sum = 0.0;
            for it in &items {
                sum += item_natural(n, it, avail, measure);
            }
            let w = sum + n.bx.gap_col * (k as f32 - 1.0).max(0.0);
            // a wrapping row fits the available width (fit-content) — sizing
            // it to the unwrapped sum made rust-lang.org's nav overflow and
            // clip its last items, wrap branch never engaging
            if n.bx.wrap && avail > 0.0 {
                w.min(avail)
            } else {
                w
            }
        }
        (Display::Flex, Dir::Column) => n
            .children
            .iter()
            .filter(|c| {
                !c.is_bg && !c.is_mask && !matches!(c.bx.pos, Pos::Absolute | Pos::Fixed)
            })
            .map(|c| natural_w(c, avail, measure))
            .fold(0.0f32, f32::max),
        (Display::Grid, _) => {
            if n.bx.cols.is_empty() {
                avail
            } else {
                tracks_w(&n.bx.cols, avail, n.bx.gap_col)
            }
        }
        // block: widest inline run or child box
        _ => {
            let mut m = 0.0f32;
            let mut run = 0.0f32;
            for c in &n.children {
                if c.is_bg
                    || c.is_mask
                    || matches!(c.bx.pos, Pos::Absolute | Pos::Fixed)
                {
                    continue; // out of flow: no contribution to shrink-to-fit
                }
                if c.is_text {
                    run += natural_w(c, avail, measure);
                } else {
                    // inline children advance the line by their horizontal
                    // margins (block()'s inline branch adds the same ml+mr
                    // while laying out), so the shrink-to-fit measure must
                    // count them or the box under-sizes and its right-hand
                    // content is advanced past by siblings
                    let w = if c.inline {
                        natural_w(c, avail, measure)
                            + resolve_len(c.bx.margin[3], avail)
                            + resolve_len(c.bx.margin[1], avail)
                    } else {
                        natural_w(c, avail, measure)
                    };
                    m = m.max(run).max(w);
                    run = 0.0;
                }
            }
            m.max(run)
        }
    }
}

fn item_natural(
    parent: &BoxNode,
    it: &Item,
    avail: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
) -> f32 {
    match it {
        Item::Text(a, b) => parent.children[*a..*b]
            .iter()
            .map(|c| natural_w(c, avail, measure))
            .sum(),
        Item::Grp(i) => {
            // a flex item's horizontal margins are real advance: the slot
            // spans the whole margin box (place() insets the item's content
            // by its own margins within the slot). Without this, margin-only
            // separators (e.g. inline menu items with `margin: 0 8px` in a
            // flex row) are dropped and neighbors lay edge to edge.
            let c = &parent.children[*i];
            natural_w(c, avail, measure)
                + resolve_len(c.bx.margin[3], avail)
                + resolve_len(c.bx.margin[1], avail)
        }
    }
}

/// Value of attribute `name` on the open tag whose text is `tag` (including
/// the leading `<`), or None.
fn attr_value<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut i = tag.find(name)?;
    // the name must start a whole attribute (preceded by whitespace)
    if i > 0 && !tag[..i].ends_with([' ', '\t', '\n', '\r']) {
        // not at an attribute boundary — try the next occurrence
        let rest = &tag[i + name.len()..];
        i += name.len() + rest.find(name)?;
        if i > 0 && !tag[..i].ends_with([' ', '\t', '\n', '\r']) {
            return None;
        }
    }
    let after = &tag[i + name.len()..];
    let after = after.trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    if let Some(q) = after.strip_prefix('"') {
        q.find('"').map(|e| &q[..e])
    } else if let Some(q) = after.strip_prefix('\'') {
        q.find('\'').map(|e| &q[..e])
    } else {
        let end = after
            .find([' ', '\t', '\n', '\r', '>'])
            .unwrap_or(after.len());
        Some(&after[..end])
    }
}

/// Merge site-CSS class-based paint into one open `tag` (whose text includes
/// the leading `<`). When a single-class rule sets a paint property (`fill`,
/// `stroke`, …) for one of the tag's classes and the tag doesn't already
/// declare it inline, append `prop: value` to the tag's `style="…"` (or add
/// one). Returns the rewritten tag text.
fn inject_tag_paint(
    tag: &str,
    classes: &str,
    sheet: &crate::css::Sheet,
    dark: bool,
) -> String {
    // paint properties an SVG rasterizer understands; only these are merged
    const PROPS: [&str; 8] = [
        "fill",
        "stroke",
        "fill-opacity",
        "stroke-opacity",
        "fill-rule",
        "stroke-width",
        "stroke-linejoin",
        "opacity",
    ];
    let mut adds: Vec<(String, String)> = Vec::new();
    for p in PROPS {
        // an inline style already setting the property wins (skip)
        if let Some(st) = attr_value(tag, "style") {
            if st.split(';').any(|d| {
                d.split_once(':').map(|(k, _)| k.trim()) == Some(p)
            }) {
                continue;
            }
        }
        // a presentation attribute on the tag itself also wins
        if attr_value(tag, p).is_some() {
            continue;
        }
        if let Some(v) = sheet.class_paint(classes, p, dark) {
            adds.push((p.to_string(), v));
        }
    }
    if adds.is_empty() {
        return tag.to_string();
    }
    let merged = adds
        .iter()
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join(";");
    match attr_value(tag, "style") {
        Some(existing) => {
            let new = format!("{existing};{merged}");
            replace_attr(tag, "style", &new)
        }
        None => {
            // insert style="…" before the closing `>` (or self-closing `/`)
            let mut out = tag.to_string();
            let ins = format!(" style=\"{merged}\"");
            match out.rfind("/>") {
                Some(i) => out.insert_str(i, &ins),
                None => {
                    let i = out.rfind('>').unwrap_or(out.len());
                    out.insert_str(i, &ins);
                }
            }
            out
        }
    }
}

/// Replace the value of attribute `name` in `tag` with `val` (re-quoted).
fn replace_attr(tag: &str, name: &str, val: &str) -> String {
    let Some(start) = tag.find(name) else {
        return tag.to_string();
    };
    let after = &tag[start + name.len()..];
    let Some(eq) = after.find('=') else {
        return tag.to_string();
    };
    let vstart = start + name.len() + eq + 1;
    let rest = tag[vstart..].trim_start();
    let qoff = tag.len() - rest.len() - vstart; // leading ws before the quote
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
    let quoted = quote.map_or(rest, |_| &rest[1..]);
    let endrel = quoted
        .find(quote.unwrap_or(' '))
        .unwrap_or(quoted.len());
    let mut out = String::with_capacity(tag.len() + val.len());
    out.push_str(&tag[..vstart + qoff]);
    if let Some(q) = quote {
        out.push(q);
    }
    out.push_str(val);
    if let Some(q) = quote {
        out.push(q);
    }
    out.push_str(&tag[vstart + qoff + quote.map_or(0, |_| 1) + endrel..]);
    out
}

/// Walk the reassembled SVG markup and merge class-based paint into each open
/// tag that carries `class="…"`. Slice-copy (never re-parse) so the raw
/// markup — including any unbalanced `<` in text — stays byte-identical
/// except for the injected styles.
fn inject_svg_class_paint(markup: &str, sheet: &crate::css::Sheet, dark: bool) -> String {
    let mut out = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after_lt = &rest[lt..];
        let Some(gt) = after_lt.find('>') else {
            out.push_str(after_lt);
            return out;
        };
        let tag = &after_lt[..=gt];
        // only open tags (not `</close>` or `<!--`) with a class attribute
        let is_open = !tag.starts_with("</") && !tag.starts_with("<!") && !tag.starts_with("<?");
        if is_open {
            if let Some(classes) = attr_value(tag, "class") {
                let rewritten = inject_tag_paint(tag, classes, sheet, dark);
                out.push_str(&rewritten);
            } else {
                out.push_str(tag);
            }
        } else {
            out.push_str(tag);
        }
        rest = &after_lt[gt + 1..];
    }
    out.push_str(rest);
    out
}

/// Resolved track widths + gaps: returns total width of all tracks+gaps.
/// shortcut: `Content` tracks measure 0 here (this path sizes a grid that is
/// itself being shrink-to-fit, where per-column item widths aren't known);
/// the `fr` tracks absorb the slack. Layout's `grid()` sizes real content.
fn tracks_w(cols: &[Track], avail: f32, gap: f32) -> f32 {
    let mut fixed = 0.0;
    let mut fr = 0.0;
    for t in cols {
        match t {
            Track::Px(p) => fixed += p,
            Track::Pct(p) => fixed += p / 100.0 * avail,
            Track::Fr(f) => fr += f,
            Track::Content => {}
        }
    }
    let gaps = gap * (cols.len() as f32 - 1.0).max(0.0);
    let free = (avail - fixed - gaps).max(0.0);
    fixed + if fr > 0.0 { free } else { 0.0 } + gaps
}

/// Resolve each track to a pixel width. `content[c]` is column `c`'s measured
/// max-content width, used only by `Track::Content` tracks (empty slice = all
/// fixed/fr, the pre-content behavior). `fr` tracks split whatever width the
/// fixed + content tracks leave over.
fn resolve_tracks(cols: &[Track], avail: f32, gap: f32, content: &[f32]) -> Vec<f32> {
    let cw = |i: usize| content.get(i).copied().unwrap_or(0.0);
    let mut fixed = 0.0;
    let mut fr = 0.0;
    for (i, t) in cols.iter().enumerate() {
        match t {
            Track::Px(p) => fixed += p,
            Track::Pct(p) => fixed += p / 100.0 * avail,
            Track::Fr(f) => fr += f,
            Track::Content => fixed += cw(i),
        }
    }
    let gaps = gap * (cols.len() as f32 - 1.0).max(0.0);
    let free = (avail - fixed - gaps).max(0.0);
    cols.iter()
        .enumerate()
        .map(|(i, t)| match t {
            Track::Px(p) => *p,
            Track::Pct(p) => p / 100.0 * avail,
            Track::Fr(f) => {
                if fr > 0.0 {
                    free * f / fr
                } else {
                    0.0
                }
            }
            Track::Content => cw(i),
        })
        .collect()
}

/// One placed float's margin box in DOC coordinates + its side.
///
/// shortcut: float scope is a downward call chain — the block() that
/// places a float keeps the list and hands it down through `place()` into
/// descendant block()s (so a nested paragraph beside an outer float still
/// narrows), but a float placed inside a nested block does NOT escape UP
/// into the parent's list: the parent's later siblings flow past it
/// (spec would re-flow them around it and force the nested block to
/// contain it). Flex/grid/absolute subtrees are handed no floats at all.
#[derive(Clone, Copy)]
struct Flt {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    /// true = `float: left`
    left: bool,
}

/// Subtract span `[a,b]` from the sorted, disjoint intervals in `iv`
/// (pieces that close up are dropped). Keeps `iv` sorted.
fn iv_sub(iv: &mut Vec<(f32, f32)>, a: f32, b: f32) {
    if b <= a {
        return;
    }
    let mut out = Vec::with_capacity(iv.len() + 1);
    for &(s, e) in iv.iter() {
        if b <= s || a >= e {
            out.push((s, e));
        } else {
            if s < a {
                out.push((s, a));
            }
            if b < e {
                out.push((b, e));
            }
        }
    }
    *iv = out;
}

/// Usable local x-interval for a text line whose doc y-span is
/// `[oy+y, oy+y+lh)`: `[0, max_width]` minus the x-spans of the floats
/// overlapping that span, picking the piece that reaches `cursor` (a
/// cursor buried inside a float continues just past it — the caller
/// clamps x into the returned range). Returns `(y_jump, x0, x1)`:
/// `y_jump > 0` means the floats cover the line's full width (zero room)
/// and the line must move down by that much first, then re-check.
///
/// An empty `flt` — every page without floats — returns
/// `(0, 0, max_width)`, the old full-width line, so those pages lay out
/// byte-identically.
fn line_excl(
    flt: &[Flt],
    ox: f32,
    oy: f32,
    y: f32,
    lh: f32,
    cursor: f32,
    max_width: f32,
) -> (f32, f32, f32) {
    if flt.is_empty() {
        return (0.0, 0.0, max_width);
    }
    let (ya, yb) = (oy + y, oy + y + lh.max(1.0));
    let mut iv = vec![(0.0f32, max_width)];
    let mut bottom = 0.0f32;
    let mut hit = false;
    for f in flt {
        if f.y0 >= yb || f.y1 <= ya {
            continue;
        }
        hit = true;
        bottom = bottom.max(f.y1);
        iv_sub(&mut iv, f.x0 - ox, f.x1 - ox);
    }
    if !hit {
        return (0.0, 0.0, max_width);
    }
    if iv.is_empty() {
        // zero room: drop the line below every blocking float (strictly
        // downward each retry, floats never move → terminates)
        return (bottom - oy - y, 0.0, 0.0);
    }
    // first piece reaching the cursor: it contains the cursor, or starts
    // just past a float that buried it
    let (a, b) = *iv
        .iter()
        .find(|(_, e)| *e >= cursor)
        .unwrap_or(iv.last().unwrap());
    (0.0, a, b)
}

/// Block flow: consecutive text leaves become wrapped line segments; child
/// boxes are placed at the running y. The 0.6*size paragraph gap is added at
/// block boundaries only - a `<br>` inside the text advances exactly one
/// line (see [`layout_from`]).
///
/// Boxes with `inline` set (UA form-control chrome, author `display:
/// inline-block`, promoted background/padding/margin inlines) join the
/// current text line instead of opening their own: shrink-to-fit width,
/// vertically centered against what is already on the line, and the text
/// after them continues on that same line.
/// shortcut: centering approximates baseline alignment (a real engine
/// aligns the inline-block's text baseline to the line's), vertical
/// padding on a promoted inline grows the line box (real engines paint
/// it without affecting line height), and a box
/// wider than its container overflows instead of shrink-wrapping its
/// content to fit (no min-content measure).
fn block(
    n: &mut BoxNode,
    measure: &dyn Fn(&str, &Style) -> f32,
    runs: &mut Vec<PositionedRun>,
    uls: &mut Vec<UR>,
    // ancestor floats (doc coords) this block's lines must avoid — handed
    // down by place(); flex/grid/absolute subtrees pass none (spec: floats
    // have no effect on flex/grid items or out-of-flow boxes)
    flt: &[Flt],
) {
    // everything visible here: ancestor floats + any this block places
    let mut floats: Vec<Flt> = flt.to_vec();
    // y = bottom of the last closed content. While `cont` is Some, the
    // current line is still open: (line_top, line_h, cursor x relative to
    // n.x); y mirrors line_top + line_h.
    let mut y = n.y;
    let mut cont: Option<(f32, f32, f32)> = None;
    // text-indent: the block's FIRST line starts `indent` px right
    // (negative shifts left; % resolves against this block's content
    // width). Consumed by whatever lays that first line — once per block().
    // shortcut: BoxStyle is per DECLARING block (spec text-indent inherits
    // into nested blocks — not modeled), a first line already carrying an
    // inline box isn't re-flowed (cont is Some → no indent), and floats/
    // out-of-flow boxes don't consume it (they lay no text line).
    let mut indent_left = true;
    let mut i = 0;
    while i < n.children.len() {
        if n.children[i].is_marker && n.style.list_pos == ListPos::Outside {
            // list-style-position: outside — the marker hangs in the list's
            // left gutter: its RIGHT edge sits `gap` left of the content box
            // (which already starts after the UA 40px list padding), on the
            // current line's top. It advances neither y nor the line cursor,
            // so the following text starts at the content edge and every
            // continuation line aligns with the FIRST LINE'S TEXT.
            // shortcut: a declared text-align on the item does not move the
            // hung marker (the text block shifts, the gutter does not).
            let m = &n.children[i];
            let w = measure(&m.text, &m.style);
            let top = cont.map_or(y, |(t, _, _)| t);
            runs.push(PositionedRun {
                x: n.x - n.style.size * 0.5 - w,
                y: top,
                text: m.text.clone(),
                style: m.style,
                width: w,
                link: m.link.clone(),
                fixed: false,
                clip: None,
                node: m.node,
                cursor: m.bx.cursor,
            });
            i += 1;
        } else if n.children[i].is_text {
            let (x0, seg_top) = match cont {
                Some((top, _, x)) => (x, top),
                None => (0.0, y),
            };
            let mut indent = 0.0;
            if indent_left {
                indent_left = false;
                if cont.is_none() {
                    indent = resolve_len(n.bx.text_indent, n.w);
                }
            }
            let mut atoms = Vec::new();
            while i < n.children.len()
                && n.children[i].is_text
                && !(n.children[i].is_marker && n.style.list_pos == ListPos::Outside)
            {
                atoms.push(leaf_atom(&n.children[i]));
                i += 1;
            }
            // exclusion origin: runs are pushed at (n.x, seg_top), so
            // layout_from's local coords map to doc space by that offset
            let (mut l, end_x) =
                layout_from(&atoms, x0, indent, n.w, measure, &floats, n.x, seg_top);
            // this batch's first-line height + last-line top (original run
            // coords, before the continuation shifts below)
            let f = l
                .runs
                .iter()
                .filter(|r| r.y == 0.0)
                .map(|r| r.style.line_height())
                .fold(0.0f32, f32::max);
            let last_orig = l.runs.last().map(|r| r.y);
            // continuing a line taller than this batch's first line (an
            // inline box leads it): center the first line's text in the
            // leftover, push wrapped lines down to the real line height
            let extra = match cont {
                Some((_, lh, _)) => (lh - f).max(0.0),
                None => 0.0,
            };
            if extra > 0.0 {
                let dy = extra * 0.5;
                for r in &mut l.runs {
                    r.y += if r.y == 0.0 { dy } else { extra };
                }
                for u in &mut l.underline_rects {
                    u.1 += if u.1 < f { dy } else { extra };
                }
            }
            let bottom = seg_top + l.height + extra;
            push_runs(l.runs, n.x, seg_top, runs);
            push_uls(l.underline_rects, n.x, seg_top, uls);
            if l.open_line {
                if let Some(lt0) = last_orig {
                    // open line top (shifted with the wrapped lines when the
                    // first line grew to the inline box's height)
                    let lt = lt0 + if lt0 > 0.0 { extra } else { 0.0 };
                    cont = Some((seg_top + lt, l.height + extra - lt, end_x));
                } else if let Some((top, lh, _)) = cont {
                    // whitespace-only continuation: line geometry unchanged,
                    // only the cursor moves
                    cont = Some((top, lh, end_x));
                }
                y = bottom;
            } else {
                cont = None;
                y = bottom;
            }
        } else if matches!(n.children[i].bx.pos, Pos::Absolute | Pos::Fixed) {
            // out of flow: sized/placed by position_all (phase 2).
            // shortcut: closes an open line first — keeps the legacy
            // paragraph gap for text interrupted by an absolute box
            if let Some((top, h, _)) = cont.take() {
                y = top + h + n.style.size * 0.6;
            }
            i += 1;
        } else if n.children[i].is_bg || n.children[i].is_mask {
            // background/mask placeholder: anchored by place(), no advance
            i += 1;
        } else if let Some(left) = match n.children[i].bx.float {
            Float::Left => Some(true),
            Float::Right => Some(false),
            Float::None => None,
        } {
            // FLOAT: out of normal flow. Placed at the current line's top
            // edge (flow y when no line is open) and recorded in `floats`;
            // it advances NEITHER y nor the open line — every later line
            // shortens around the rect (layout_from's line exclusion),
            // `clear` on the float drops it below the floats it names.
            // shortcut: runs already laid on the open line are not
            // re-flowed around the float (spec re-lays the line — those
            // runs may overlap the rect; only content laid AFTER the float
            // avoids it), and a float taller than the following content
            // overflows this block's bottom instead of being contained.
            let mut top = cont.map_or(y, |(t, _, _)| t);
            for f in &floats {
                let named = match n.children[i].bx.clear {
                    Clear::Left => f.left,
                    Clear::Right => !f.left,
                    Clear::All => true,
                    Clear::None => false,
                };
                if named {
                    top = top.max(f.y1);
                }
            }
            let ml = resolve_len(n.children[i].bx.margin[3], n.w);
            let mr = resolve_len(n.children[i].bx.margin[1], n.w);
            let mt = resolve_len(n.children[i].bx.margin[0], n.w);
            // shrink-to-fit: max-content OUTER width (incl. margins), then
            // clamped to the space on our side at `top`; fully blocked →
            // drop below every blocking float and retry (top strictly
            // grows each pass → terminates)
            let prefer = natural_w(&n.children[i], n.w, measure) + ml + mr;
            let (s, e) = loop {
                let mut iv = vec![(0.0f32, n.w)];
                let mut drop = 0.0f32;
                for f in &floats {
                    if f.y0 > top || f.y1 <= top {
                        continue;
                    }
                    drop = drop.max(f.y1);
                    iv_sub(&mut iv, f.x0 - n.x, f.x1 - n.x);
                }
                // our side's piece: lowest gap for float:left, highest for
                // float:right
                let pick = if left { iv.first().copied() } else { iv.last().copied() };
                match pick {
                    None => top = drop, // no gap at this y → try below
                    Some(gap) => break gap,
                }
            };
            // shortcut: no min-content floor — the float shrinks into
            // whatever gap exists (even a sliver) instead of dropping
            // below like spec; a DECLARED width wider than that gap (and a
            // %-width, resolved here against the gap, not the containing
            // block) overflows it instead
            let fw = prefer.min(e - s);
            let mx = if left { s } else { e - fw };
            let adv =
                place(&mut n.children[i], n.x + mx, top, fw, measure, runs, uls, &floats);
            // record the margin box — what line boxes must avoid
            let fy0 = n.children[i].y - mt;
            floats.push(Flt {
                x0: n.children[i].x - ml,
                y0: fy0,
                x1: n.children[i].x + n.children[i].w + mr,
                y1: fy0 + adv,
                left,
            });
            i += 1;
        } else if n.children[i].inline {
            // this inline box starts (or continues) the block's first line
            // — the indent slot is spent either way
            indent_left = false;
            // inline-level box (form control / author inline-block /
            // promoted background inline): shrink-to-fit, joins the
            // current line. shortcut: not re-flowed around floats — only
            // TEXT lines narrow (spec runs line-box exclusion here too),
            // so an inline box may sit under a float placed on its line.
            // A %-width resolves against the CONTAINING
            // BLOCK (this block's content width) per CSS - convert to px
            // here so place()'s re-resolution is exact (width:100%
            // buttons fill their container, not their own text)
            if let Some(Len::Pct(p)) = n.children[i].bx.width {
                n.children[i].bx.width = Some(Len::Px(p / 100.0 * n.w));
            }
            let (mut line_top, mut line_h, mut cx) = cont.unwrap_or((y, 0.0, 0.0));
            let mut avail = (n.w - cx).max(0.0);
            let mut bw = natural_w(&n.children[i], avail, measure);
            let mut ml = resolve_len(n.children[i].bx.margin[3], avail);
            let mut mr = resolve_len(n.children[i].bx.margin[1], avail);
            if cx > 0.0 && cx + ml + bw + mr > n.w {
                // no room left on this line → wrap to a fresh one
                y = line_top + line_h;
                line_top = y;
                line_h = 0.0;
                cx = 0.0;
                avail = n.w;
                bw = natural_w(&n.children[i], avail, measure);
                ml = resolve_len(n.children[i].bx.margin[3], avail);
                mr = resolve_len(n.children[i].bx.margin[1], avail);
            }
            // place() derives the border box from the width it is handed
            // (px widths — incl. the UA 170px input default — land exactly;
            // shortcut: a %-width on an inline control resolves against the
            // line's remainder instead of the parent width)
            let outer = bw + ml + mr;
            ml = resolve_len(n.children[i].bx.margin[3], outer);
            mr = resolve_len(n.children[i].bx.margin[1], outer);
            let adv = place(&mut n.children[i], n.x + cx, line_top, outer, measure, runs, uls, &floats);
            let va = n.children[i].bx.valign;
            if va == VAlign::Baseline {
                // baseline/absent: today's placement, byte-identical —
                // centered against what is already on the line
                if line_h > 0.0 && adv < line_h {
                    let dy = (line_h - adv) * 0.5;
                    shift_tree(&mut n.children[i], 0.0, dy);
                    let (a, b) = n.children[i].run_range;
                    for r in &mut runs[a as usize..b as usize] {
                        r.y += dy;
                    }
                    let (a, b) = n.children[i].ul_range;
                    for u in &mut uls[a as usize..b as usize] {
                        u.1 += dy;
                    }
                } else {
                    line_h = line_h.max(adv);
                }
            } else {
                // vertical-align on the inline-level box. The line's
                // baseline is estimated from THIS block's own font —
                // shortcut: fixed metrics (no font tables): 0.8em ascent,
                // 0.5em x-height (so `middle` = baseline − x-height/2),
                // sub/super = ±0.2/0.3em; a length's % resolves against
                // the containing width, not the line height; alignment is
                // measured at placement time (a box that later makes the
                // line taller doesn't re-align), and the line box only
                // grows DOWNWARD — a raised box may paint above line_top.
                let fs = n.style.size;
                let lh = line_h.max(adv);
                let dy = match va {
                    VAlign::Top => 0.0, // box top is already the line top
                    VAlign::Bottom => lh - adv,
                    VAlign::Middle => 0.8 * fs - 0.25 * fs - adv * 0.5,
                    VAlign::Sub => 0.2 * fs,
                    VAlign::Super => -0.3 * fs,
                    VAlign::Len(l) => -resolve_len(l, n.w),
                    VAlign::Baseline => 0.0, // handled above
                };
                if dy != 0.0 {
                    shift_tree(&mut n.children[i], 0.0, dy);
                    let (a, b) = n.children[i].run_range;
                    for r in &mut runs[a as usize..b as usize] {
                        r.y += dy;
                    }
                    let (a, b) = n.children[i].ul_range;
                    for u in &mut uls[a as usize..b as usize] {
                        u.1 += dy;
                    }
                }
                // the line box keeps containing the box's bottom edge
                // (below line_top); it never grows upward — the raised
                // part simply overflows above the line
                line_h = lh.max(dy + adv);
            }
            cx += ml + n.children[i].w + mr;
            y = line_top + line_h;
            cont = Some((line_top, line_h, cx));
            i += 1;
        } else {
            // regular child box: close the open line + paragraph gap first
            // (a nested block occupies the block's top — text below it is
            // not the first line, so the indent slot is spent)
            indent_left = false;
            if let Some((top, h, _)) = cont.take() {
                y = top + h + n.style.size * 0.6;
            }
            // `clear` (spec §9.5.2): start below every float this box
            // names — ancestor floats included (they ride in `floats`)
            y = match n.children[i].bx.clear {
                Clear::None => y,
                cl => floats.iter().fold(y, |acc, f| {
                    let named = match cl {
                        Clear::Left => f.left,
                        Clear::Right => !f.left,
                        _ => true, // Clear::All
                    };
                    if named {
                        acc.max(f.y1)
                    } else {
                        acc
                    }
                }),
            };
            let adv = place(&mut n.children[i], n.x, y, n.w, measure, runs, uls, &floats);
            y += adv;
            if n.children[i].open {
                // child's trailing break: gap styled by the child itself
                y += n.children[i].style.size * 0.6;
            }
            i += 1;
        }
    }
    if let Some((top, h, _)) = cont {
        y = top + h;
    }
    n.h = y - n.y;
    n.open = cont.is_some();
}

/// Flex row: max-content sizing with proportional shrink on overflow,
/// justify distributes free space, cross-axis align shifts items into the
/// row height. shortcut: no wrap, no grow/basis, stretch = start.
fn flex_row(n: &mut BoxNode, measure: &dyn Fn(&str, &Style) -> f32, runs: &mut Vec<PositionedRun>, uls: &mut Vec<UR>) {
    let items = items_of(&n.children);
    let k = items.len();
    if k == 0 {
        n.h = 0.0;
        n.open = false;
        return;
    }
    let gap = n.bx.gap_col;
    let gaps = gap * (k as f32 - 1.0).max(0.0);
    let mut widths: Vec<f32> = items.iter().map(|it| item_natural(n, it, n.w, measure)).collect();
    // A % width on a flex item is its flex basis: it resolves against the
    // row's content width, but the row still owes the other items their
    // own widths — cap the margin box at the space the siblings leave and
    // floor it at the item's intrinsic size (real engines' min-width:auto
    // clamp). Without the cap a `width:100%` list (the wiki article-tools
    // tabs beside the Tools menu) measured into justify-content's pocket
    // and slid the sibling onto its end; without the basis side-by-side
    // `%` column items sized from intrinsic text width instead of the row.
    for (idx, it) in items.iter().enumerate() {
        if let Item::Grp(i) = it {
            if let Some(Len::Pct(p)) = n.children[*i].bx.width {
                let others: f32 = widths
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != idx)
                    .map(|(_, v)| *v)
                    .sum();
                let mlr = resolve_len(n.children[*i].bx.margin[3], n.w)
                    + resolve_len(n.children[*i].bx.margin[1], n.w);
                let basis = (p / 100.0 * n.w + mlr).max(widths[idx]);
                let cap = (n.w - gaps - others).max(widths[idx]);
                widths[idx] = basis.min(cap);
            }
        }
    }
    let natural_sum: f32 = widths.iter().sum();
    // nowrap shrinks to fit; wrap keeps natural widths and breaks lines
    if !n.bx.wrap && natural_sum + gaps > n.w && natural_sum > 0.0 {
        let scale = ((n.w - gaps).max(0.0)) / natural_sum;
        for w in &mut widths {
            *w *= scale;
        }
    }
    // wrap: greedily split items into lines that fit the content width
    let lines: Vec<std::ops::Range<usize>> = if n.bx.wrap {
        let mut out = Vec::new();
        let (mut start, mut used) = (0usize, 0.0f32);
        for i in 0..k {
            let need = widths[i] + if i > start { gap } else { 0.0 };
            if i > start && used + need > n.w {
                out.push(start..i);
                start = i;
                used = 0.0;
            }
            used += need;
        }
        out.push(start..k);
        out
    } else {
        vec![0..k]
    };
    let mut y = n.y;
    let mut total_h = 0.0f32;
    let mut last_open = false;
    // convert a % item's width to the slot's exact px: the row reserved
    // exactly this width (the basis cap above), so placing must not
    // re-resolve the % against the container and inflate past the slot —
    // that re-resolution was what slid justified siblings onto the item
    for (idx, it) in items.iter().enumerate() {
        if let Item::Grp(i) = it {
            if let Some(Len::Pct(_)) = n.children[*i].bx.width {
                let ml = resolve_len(n.children[*i].bx.margin[3], widths[idx]);
                let mr = resolve_len(n.children[*i].bx.margin[1], widths[idx]);
                n.children[*i].bx.width = Some(Len::Px(as_content_decl(
                    (widths[idx] - ml - mr).max(0.0),
                    &n.children[*i].bx,
                    widths[idx],
                )));
            }
        }
    }
    for (li, line) in lines.iter().enumerate() {
        if li > 0 {
            y += n.bx.gap_row; // row-gap between wrapped lines
        }
        let lh = flex_line(n, &items, &widths, line.clone(), y, gap, measure, runs, uls, &mut last_open);
        y += lh;
        total_h += lh + if li > 0 { n.bx.gap_row } else { 0.0 };
    }
    n.h = total_h;
    n.open = last_open;
}

/// Place one flex line (justify + cross-axis align); returns the line height.
#[allow(clippy::too_many_arguments)]
fn flex_line(
    n: &mut BoxNode,
    items: &[Item],
    widths: &[f32],
    line: std::ops::Range<usize>,
    y: f32,
    gap: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
    runs: &mut Vec<PositionedRun>,
    uls: &mut Vec<UR>,
    last_open: &mut bool,
) -> f32 {
    let k = line.len();
    let used: f32 = line.clone().map(|i| widths[i]).sum::<f32>() + gap * (k as f32 - 1.0).max(0.0);
    let free = (n.w - used).max(0.0);
    let (x_off, extra) = match n.bx.justify {
        Justify::Center => (free * 0.5, 0.0),
        Justify::End => (free, 0.0),
        Justify::SpaceBetween if k > 1 => (0.0, free / (k as f32 - 1.0)),
        _ => (0.0, 0.0), // Start
    };
    let mut row_h = 0.0f32;
    let mut x = n.x + x_off;
    let mut marks: Vec<(usize, usize, f32)> = Vec::with_capacity(k); // run_start, ul_start, h
    for idx in line.clone() {
        let w = widths[idx];
        let rs = runs.len();
        let us = uls.len();
        let (ih, io) = match items[idx] {
            Item::Text(a, b) => {
                let mut atoms = Vec::new();
                for c in &n.children[a..b] {
                    atoms.push(leaf_atom(c));
                }
                let l = layout(&atoms, w, measure);
                push_runs(l.runs, x, y, runs);
                push_uls(l.underline_rects, x, y, uls);
                (l.height, l.open_line)
            }
            Item::Grp(i) => {
                // a % item's width was already converted to the row's exact
                // reserved px above, so the slot width is the containing
                // block measure — no re-resolution here
                let adv = place(&mut n.children[i], x, y, w, measure, runs, uls, &[]);
                (adv, n.children[i].open) // cross-size = margin box
            }
        };
        marks.push((rs, us, ih));
        row_h = row_h.max(ih);
        *last_open = io;
        x += w + gap + extra;
    }
    // cross-axis align: shift each item (runs slice + box coords) into row_h
    for (idx, &(rs, us, h)) in marks.iter().enumerate() {
        let dy = match n.bx.align {
            Align::Center => (row_h - h) * 0.5,
            Align::End => row_h - h,
            _ => 0.0, // Start / Stretch (stretch: shortcut, no reflow)
        };
        if dy == 0.0 {
            continue;
        }
        let end = marks.get(idx + 1).map(|m| m.0).unwrap_or(runs.len());
        let uend = marks.get(idx + 1).map(|m| m.1).unwrap_or(uls.len());
        for r in &mut runs[rs..end] {
            r.y += dy;
        }
        for u in &mut uls[us..uend] {
            u.1 += dy;
        }
        if let Item::Grp(i) = items[line.start + idx] {
            shift_tree(&mut n.children[i], 0.0, dy);
        }
    }
    row_h
}

/// Flex column: stack items with row-gap. shortcut: justify is a no-op (no
/// height prop yet); text items lay full-width (text-align centers them),
/// box items honor align-items on the x axis.
fn flex_col(n: &mut BoxNode, measure: &dyn Fn(&str, &Style) -> f32, runs: &mut Vec<PositionedRun>, uls: &mut Vec<UR>) {
    let items = items_of(&n.children);
    if items.is_empty() {
        n.h = 0.0;
        n.open = false;
        return;
    }
    let mut y = n.y;
    let mut last_open = false;
    for (idx, it) in items.iter().enumerate() {
        if idx > 0 {
            y += n.bx.gap_row;
        }
        let rs = runs.len();
        let us = uls.len();
        let (ih, io) = match it {
            Item::Text(a, b) => {
                let mut atoms = Vec::new();
                for c in &n.children[*a..*b] {
                    atoms.push(leaf_atom(c));
                }
                let l = layout(&atoms, n.w, measure);
                push_runs(l.runs, n.x, y, runs);
                push_uls(l.underline_rects, n.x, y, uls);
                (l.height, l.open_line)
            }
            Item::Grp(i) => {
                let adv = place(&mut n.children[*i], n.x, y, n.w, measure, runs, uls, &[]);
                let dx = match n.bx.align {
                    Align::Center => (n.w - n.children[*i].w) * 0.5,
                    Align::End => n.w - n.children[*i].w,
                    _ => 0.0,
                };
                if dx != 0.0 {
                    shift_tree(&mut n.children[*i], dx, 0.0);
                    for r in &mut runs[rs..] {
                        r.x += dx;
                    }
                    for u in &mut uls[us..] {
                        u.0 += dx;
                    }
                }
                (adv, n.children[*i].open)
            }
        };
        y += ih;
        last_open = io;
    }
    n.h = y - n.y;
    n.open = last_open;
}

/// Grid: cells over resolved tracks; row height = tallest cell, row-gap
/// between rows. Items with `grid-column: <name>` place into the
/// [name-start]…[name-end] column span (stacking in successive rows);
/// auto items flow row-major. shortcut: named and auto items can share a
/// row index (overlap) — real grids resolve full placement sparsely.
fn grid(n: &mut BoxNode, measure: &dyn Fn(&str, &Style) -> f32, runs: &mut Vec<PositionedRun>, uls: &mut Vec<UR>) {
    let items = items_of(&n.children);
    if items.is_empty() {
        n.h = 0.0;
        n.open = false;
        return;
    }
    let nc = if n.bx.cols.is_empty() { 1 } else { n.bx.cols.len() };
    // resolve each item's (row, c0, c1) — needs only the column COUNT, so it
    // runs BEFORE track widths are resolved (content tracks are sized from
    // the items placed in them)
    let resolve_col = |name: &str| -> Option<(usize, usize)> {
        let line = |suffix: &str| {
            n.bx
                .col_lines
                .iter()
                .find(|(l, _)| l == name || l == &format!("{name}{suffix}"))
                .map(|(_, i)| *i)
        };
        let s = line("-start")?;
        let e = line("-end").filter(|&e| e > s).unwrap_or(s + 1);
        Some((s, e.min(nc)))
    };
    let mut col_row = vec![0usize; nc]; // next free row per column (named)
    let mut ar = 0usize; // auto cursor row
    let mut ac = 0usize; // auto cursor col
    let mut spots: Vec<(usize, usize, usize)> = Vec::with_capacity(items.len());
    for it in &items {
        let named = match it {
            Item::Grp(i) => n.children[*i]
                .bx
                .grid_col
                .as_deref()
                .and_then(resolve_col),
            Item::Text(..) => None,
        };
        spots.push(match named {
            Some((s, e)) => {
                let r = (s..e).map(|c| col_row[c]).max().unwrap_or(0);
                for c in s..e {
                    col_row[c] = r + 1;
                }
                (r, s, e)
            }
            None => {
                let sp = (ar, ac, (ac + 1).min(nc));
                ac += 1;
                if ac >= nc {
                    ac = 0;
                    ar += 1;
                }
                sp
            }
        });
    }
    // per-column max-content width for `min-content`/`max-content`/`auto`
    // tracks: the widest item placed in the column. shortcut: a multi-column
    // span contributes its full width to each column it covers (over-allocates
    // a spanning content track; rare in practice).
    let content: Vec<f32> = if n.bx.cols.iter().any(|t| *t == Track::Content) {
        let mut c = vec![0.0f32; nc];
        for (i, it) in items.iter().enumerate() {
            let (_, c0, c1) = spots[i];
            let w = item_natural(n, it, n.w, measure);
            for col in c0..c1.min(nc) {
                c[col] = c[col].max(w);
            }
        }
        c
    } else {
        Vec::new()
    };
    let cols: Vec<f32> = if n.bx.cols.is_empty() {
        vec![n.w]
    } else {
        resolve_tracks(&n.bx.cols, n.w, n.bx.gap_col, &content)
    };
    // x offset + width of columns [c0, c1)
    let span_xw = |c0: usize, c1: usize| {
        let x: f32 = cols[..c0].iter().sum::<f32>() + n.bx.gap_col * c0 as f32;
        let w: f32 = cols[c0..c1].iter().sum::<f32>() + n.bx.gap_col * (c1 - c0 - 1) as f32;
        (x, w)
    };
    let nrows = spots.iter().map(|s| s.0).max().unwrap_or(0) + 1;
    let mut y = n.y;
    let mut last_open = false;
    for r in 0..nrows {
        let mut row_h = 0.0f32;
        let mut marks: Vec<(usize, usize, f32)> = Vec::with_capacity(nc); // run_start, ul_start, h
        let mut row_items: Vec<usize> = (0..items.len()).filter(|&i| spots[i].0 == r).collect();
        row_items.sort_by_key(|&i| spots[i].1);
        let mut cell_ios: Vec<bool> = Vec::with_capacity(nc);
        for &idx in &row_items {
            let (_, c0, c1) = spots[idx];
            let (dx, w) = span_xw(c0, c1);
            let x = n.x + dx;
            let rs = runs.len();
            let us = uls.len();
            let (ih, io) = match items[idx] {
                Item::Text(a, b) => {
                    let mut atoms = Vec::new();
                    for ch in &n.children[a..b] {
                        atoms.push(leaf_atom(ch));
                    }
                    let l = layout(&atoms, w, measure);
                    push_runs(l.runs, x, y, runs);
                    push_uls(l.underline_rects, x, y, uls);
                    (l.height, l.open_line)
                }
                Item::Grp(i) => {
                    let adv = place(&mut n.children[i], x, y, w, measure, runs, uls, &[]);
                    (adv, n.children[i].open)
                }
            };
            marks.push((rs, us, ih));
            cell_ios.push(io);
            row_h = row_h.max(ih);
        }
        // cross-axis align within the row (ranges are safe: the next row
        // hasn't been laid yet)
        for (ci, &(rs, us, h)) in marks.iter().enumerate() {
            let dy = match n.bx.align {
                Align::Center => (row_h - h) * 0.5,
                Align::End => row_h - h,
                _ => 0.0,
            };
            if dy == 0.0 {
                continue;
            }
            let end = marks.get(ci + 1).map(|m| m.0).unwrap_or(runs.len());
            let uend = marks.get(ci + 1).map(|m| m.1).unwrap_or(uls.len());
            for rr in &mut runs[rs..end] {
                rr.y += dy;
            }
            for u in &mut uls[us..uend] {
                u.1 += dy;
            }
            if let Item::Grp(i) = items[row_items[ci]] {
                shift_tree(&mut n.children[i], 0.0, dy);
            }
        }
        last_open = cell_ios.last().copied().unwrap_or(false);
        y += row_h;
        if r + 1 < nrows {
            y += n.bx.gap_row;
        }
    }
    n.h = y - n.y;
    n.open = last_open;
}
pub fn layout(atoms: &[Atom], max_width: f32, measure: &dyn Fn(&str, &Style) -> f32) -> Layout {
    layout_from(atoms, 0.0, 0.0, max_width, measure, &[], 0.0, 0.0).0
}

/// [`layout`], but the first line starts at `x0` — text continuing on a
/// line that already holds an inline box — and `indent` shifts a FRESH
/// first line right by that many px (text-indent; 0 for every caller
/// except block()'s first line). Returns the layout plus the line's end x
/// (the continuation cursor for the next inline content).
///
/// `flt`/`ox`/`oy` = floats to wrap around and the doc offset of this
/// batch's local origin (block() passes them; `layout()` passes none).
/// shortcut: a continuation line's EXISTING height isn't visible here —
/// float exclusion for its first line uses the first word's line height —
/// and `text-align` still centers against the full `max_width`, so a
/// centered/right-aligned line beside a float may overlap it (left-aligned
/// text, the default, is exact).
#[allow(clippy::too_many_arguments)]
fn layout_from(
    atoms: &[Atom],
    x0: f32,
    indent: f32,
    max_width: f32,
    measure: &dyn Fn(&str, &Style) -> f32,
    flt: &[Flt],
    ox: f32,
    oy: f32,
) -> (Layout, f32) {
    let mut runs = Vec::new();
    let mut underline_rects = Vec::new();
    let mut y = 0.0f32;
    let mut x = x0 + indent;
    let mut line_height = 0.0f32;

    fn word(
        text: &str,
        style: Style,
        link: &Option<std::rc::Rc<str>>,
        node: Option<u32>,
        cursor: Option<Cursor>,
    ) -> Atom {
        Atom {
            text: text.into(),
            style,
            hard_break: false,
            link: link.clone(),
            node,
            cursor,
        }
    }

    fn br(style: Style) -> Atom {
        // a hard break emits no run, so it carries no hover target
        Atom {
            text: String::new(),
            style,
            hard_break: true,
            link: None,
            node: None,
            cursor: None,
        }
    }

    // collapse path: split on any whitespace, remembering which had a
    // trailing space (empty split pieces at the boundaries ARE meaningful:
    // they mean this atom starts/ends with whitespace that must separate it
    // from the neighboring inline element's text). The long-standing
    // normal/nowrap/pre-line behavior — byte-identical when no
    // `white-space` value is declared.
    fn push_words(
        text: &str,
        style: Style,
        link: &Option<std::rc::Rc<str>>,
        node: Option<u32>,
        cursor: Option<Cursor>,
        out: &mut Vec<Atom>,
    ) {
        let mut first = true;
        let mut pending_ws = false;
        for w in text.split(|c: char| c.is_whitespace()) {
            if w.is_empty() {
                pending_ws = true;
                continue;
            }
            if !first || pending_ws {
                out.push(word(" ", style, link, node, cursor));
            }
            first = false;
            pending_ws = false;
            out.push(word(w, style, link, node, cursor));
        }
        // text ends with whitespace → separator before the next atom's text
        // (a space atom at line start is a no-op: the draw loop only
        // advances x when x > line_x0)
        if pending_ws {
            out.push(word(" ", style, link, node, cursor));
        }
    }

    // preserve path (pre-wrap): words + EXACT whitespace runs kept as
    // their own atoms ("a  b" → "a", "  ", "b") — nothing is folded, so
    // double spaces survive into the runs and the drawing
    fn push_preserved(
        text: &str,
        style: Style,
        link: &Option<std::rc::Rc<str>>,
        node: Option<u32>,
        cursor: Option<Cursor>,
        out: &mut Vec<Atom>,
    ) {
        let mut it = text.char_indices().peekable();
        while let Some((s, c)) = it.next() {
            let is_ws = c.is_whitespace();
            let mut e = s + c.len_utf8();
            while let Some(&(n, nc)) = it.peek() {
                if nc.is_whitespace() != is_ws {
                    break;
                }
                e = n + nc.len_utf8();
                it.next();
            }
            out.push(word(&text[s..e], style, link, node, cursor));
        }
    }

    // Whitespace text for a `pre`/`pre-wrap` atom: spaces kept, `\t` → one
    // space (no tab stops in this engine — a raw tab would measure/draw a
    // .notdef box glyph), `\r` dropped (the CR half of a CRLF pair; the
    // paired `\n` already made the line break — a LONE `\r` renders as
    // nothing, shortcut). Other Unicode whitespace passes through.
    fn pre_ws(t: &str) -> String {
        let mut s = String::with_capacity(t.len());
        for c in t.chars() {
            match c {
                '\r' => {}
                '\t' => s.push(' '),
                c => s.push(c),
            }
        }
        s
    }

    // split text into word atoms first so wrapping is per-word; the
    // element's white-space mode picks the splitter (see [`Ws`]):
    // normal/nowrap fold every whitespace run into one separator like
    // before, pre-line folds but breaks at `\n`, pre keeps each
    // `\n`-separated line as ONE un-wrappable atom with its spaces intact,
    // pre-wrap keeps the spaces AND wraps at them
    let mut words: Vec<Atom> = Vec::new();
    for a in atoms {
        if a.hard_break {
            words.push(br(a.style));
            continue;
        }
        if a.style.ws.honors_newline() {
            // `\n` becomes a hard break — split first, then each segment
            // through its mode's splitter (existing hard_break machinery;
            // a LEADING break stays the documented no-op)
            for (k, seg) in a.text.split('\n').enumerate() {
                if k > 0 {
                    words.push(br(a.style));
                }
                match a.style.ws {
                    Ws::Pre => {
                        let seg = pre_ws(seg);
                        if !seg.is_empty() {
                            words.push(word(&seg, a.style, &a.link, a.node, a.cursor));
                        }
                    }
                    Ws::PreWrap => push_preserved(
                        &pre_ws(seg),
                        a.style,
                        &a.link,
                        a.node,
                        a.cursor,
                        &mut words,
                    ),
                    _ => push_words(seg, a.style, &a.link, a.node, a.cursor, &mut words), // pre-line
                }
            }
        } else {
            push_words(&a.text, a.style, &a.link, a.node, a.cursor, &mut words);
        }
    }

    let space_w = |s: &Style| measure(" ", s) + spacing_extra(" ", s);

    // `<br>` advances exactly ONE line at the current line-height - no
    // paragraph gap (a 0.6*size gap here spaced e.g. a hero h1's lines ~2x
    // normal). Consecutive `<br>`s each add one empty line, the classic
    // blank-line-per-break behavior. shortcut: a LEADING `<br>` (empty line
    // before any content) is still a no-op.
    let mut after_br = false;

    // usable x-range of the line being built. The empty-float list keeps
    // both at the old full-width values forever (`line_x0` = 0 ≡ the old
    // `x > 0.0` guards), so pages without floats lay out byte-identically.
    // An indented first line's left edge IS the indent: leading source
    // whitespace drops there like at any line start, and after a wrap/hard
    // break line_x0 returns to 0 (text-indent is first-line only).
    let mut line_x0 = if indent != 0.0 { indent } else { 0.0 };
    let mut line_max = max_width;

    for w in &words {
        if w.hard_break {
            // "the line has content" = the cursor sits past the line's
            // left edge — identical to the old `x > 0.0` whenever
            // line_x0 is 0 (no indent/floats), and it also closes a
            // NEGATIVE-indented line whose cursor never reaches 0
            if x > line_x0 {
                y += line_height;
                x = 0.0;
                line_x0 = 0.0;
                line_height = 0.0;
                after_br = true;
            } else if after_br {
                // consecutive <br>: the line is empty - add it
                y += w.style.line_height();
            }
            continue;
        }
        // Collapsed separator (normal/nowrap/pre-line): advance only when
        // the line already has content to its left — a space at line start
        // is dropped. pre/pre-wrap whitespace is CONTENT, not a separator:
        // it falls through as a real run below, preserved even at line
        // start (that is the point of `pre`).
        if w.text == " " && !w.style.ws.preserve() {
            if x > line_x0 {
                x += space_w(&w.style);
            }
            continue;
        }
        // measured width + tracking in ONE number: spacing_extra is the
        // same rule draw_text replays per glyph (main.rs), so what wraps
        // here is exactly what paints there
        let ww = measure(&w.text, &w.style) + spacing_extra(&w.text, &w.style);
        if x > line_x0 && x + ww > line_max && w.style.ws.wrap() {
            y += line_height;
            x = 0.0;
            line_x0 = 0.0; // wrapped line starts at the box edge (indent is first-line only)
            line_height = 0.0;
        }
        after_br = false; // real content ends the <br> run
        if line_height == 0.0 && !flt.is_empty() {
            // the line is starting (first word or just wrapped): take its
            // float exclusion for this y — shortcut: measured with THIS
            // word's line height, so a later-taller line (bigger font
            // mid-line) doesn't re-check against the floats. A fully
            // blocked line drops below the floats and re-checks (monotonic
            // — see line_excl), then x clamps into the free piece (a
            // cursor buried in a float continues just past it).
            loop {
                let (jump, a, b) = line_excl(flt, ox, oy, y, w.style.line_height(), x, max_width);
                if jump == 0.0 {
                    line_x0 = a;
                    line_max = b;
                    x = x.clamp(a, b);
                    break;
                }
                y += jump;
            }
        }
        let lh = w.style.line_height();
        runs.push(PositionedRun {
            x,
            y,
            text: w.text.clone(),
            style: w.style,
            width: ww,
            link: w.link.clone(),
            fixed: false,
            clip: None,
            node: w.node,
            cursor: w.cursor,
        });
        if w.style.underline {
            underline_rects.push((x, y + lh - 2.0, ww, 1.0, w.style.color, false));
        }
        x += ww;
        line_height = line_height.max(lh);
    }
    y += line_height;

    // text-align: shift each completed line inside max_width
    let mut i = 0;
    while i < runs.len() {
        let y0 = runs[i].y;
        let align = runs[i].style.text_align;
        let sz = runs[i].style.size;
        let mut j = i;
        let mut lw = 0.0f32;
        while j < runs.len() && runs[j].y == y0 {
            lw = lw.max(runs[j].x + runs[j].width);
            j += 1;
        }
        let dx = match align {
            1 => (max_width - lw) * 0.5,
            2 => max_width - lw,
            _ => 0.0,
        };
        if dx != 0.0 {
            for r in &mut runs[i..j] {
                r.x += dx;
            }
            let band = y0 + sz * 1.4;
            for u in underline_rects.iter_mut() {
                if u.1 >= y0 && u.1 < band {
                    u.0 += dx;
                }
            }
        }
        i = j;
    }

    (
        Layout {
            runs,
            height: y,
            underline_rects,
            rects: Vec::new(),
            images: Vec::new(),
            shadows: Vec::new(),
            open_line: x > line_x0,
            // text-only path (inline continuation): no box tree was laid
            // → no form controls, no focus, no values, no hover targets
            forms: Vec::new(),
            focused: None,
            form_values: HashMap::new(),
            sticky: Vec::new(),
            hover_targets: Vec::new(),
        },
        x,
    )
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
            node: None,
            cursor: None,
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
    fn inline_boundary_spaces_survive() {
        // "create " + <strong>"high-quality"</strong> + " with ..." — trailing/
        // leading whitespace at atom edges must become space atoms (regression:
        // rendered as "createhigh-quality ... applicationswith")
        let atoms = vec![
            mk_atom("to create ", false),
            mk_atom("high-quality", false),
            mk_atom(" with power", false),
        ];
        let l = layout(&atoms, 500.0, &|s, _| s.len() as f32 * 4.0);
        let texts: Vec<&str> = l.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["to", "create", "high-quality", "with", "power"]);
        // "create" and "high-quality" are separated by actual x space
        let create = l.runs.iter().find(|r| r.text == "create").unwrap();
        let hq = l.runs.iter().find(|r| r.text == "high-quality").unwrap();
        assert!(hq.x >= create.x + create.width, "no gap before strong");
        // whitespace-only atom (between two inline elements) also separates
        let l2 = layout(&[mk_atom("a", false), mk_atom(" ", false), mk_atom("b", false)], 500.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l2.runs.len(), 2);
        assert!(l2.runs[1].x > l2.runs[0].x + l2.runs[0].width, "ws-only atom separates");
    }

    #[test]
    fn hard_break_starts_new_line() {
        let atoms = vec![mk_atom("a", false), mk_atom("", true), mk_atom("b", false)];
        let l = layout(&atoms, 100.0, &|s, _| s.len() as f32 * 4.0);
        assert_eq!(l.runs.len(), 2);
        assert!(l.runs[1].y > l.runs[0].y);
    }

    #[test]
    fn br_advances_exactly_one_line_height() {
        // <br> between two text runs advances exactly ONE line at the
        // current line-height - no legacy 0.6*size paragraph gap (a hero
        // h1's lines were spaced ~2x normal with it)
        let mut n = BoxNode::group(Style::new(), BoxStyle::default());
        n.children = vec![
            leaf("one"),
            BoxNode::leaf(Style::new(), String::new(), true, None), // <br>
            leaf("two"),
        ];
        let l = layout_tree(&mut n, 200.0, &m4);
        assert_eq!(l.runs.len(), 2);
        let line = 15.0 * 1.35;
        assert!(
            (l.runs[1].y - line).abs() < 0.01,
            "y={} = exactly one line, no 0.6*size gap",
            l.runs[1].y
        );
        assert!((n.h - line * 2.0).abs() < 0.01, "h={} = two lines", n.h);
    }

    #[test]
    fn consecutive_br_each_add_one_empty_line() {
        // classic behavior: each <br> beyond the first in a run adds one
        // empty line - a<br><br>b puts b two lines below a, three <br>s
        // put it three lines below
        let mk = |brs: usize| {
            let mut n = BoxNode::group(Style::new(), BoxStyle::default());
            n.children = vec![leaf("a")];
            for _ in 0..brs {
                n.children.push(BoxNode::leaf(Style::new(), String::new(), true, None));
            }
            n.children.push(leaf("b"));
            n
        };
        let line = 15.0 * 1.35;
        for (brs, lines) in [(2usize, 2.0f32), (3, 3.0)] {
            let mut n = mk(brs);
            let l = layout_tree(&mut n, 200.0, &m4);
            let b = l.runs.iter().find(|r| r.text == "b").expect("b run");
            assert!(
                (b.y - line * lines).abs() < 0.01,
                "{brs} brs: y={} expect {}",
                b.y,
                line * lines
            );
        }
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

    // ---- box tree (Track A) ----

    /// 4px/char measure shared by the box tests.
    fn m4(s: &str, _st: &Style) -> f32 {
        s.len() as f32 * 4.0
    }

    fn leaf(text: &str) -> BoxNode {
        BoxNode::leaf(Style::new(), text.into(), false, None)
    }

    fn flex_row(children: Vec<BoxNode>, bx: BoxStyle) -> BoxNode {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Flex,
                dir: Dir::Row,
                ..bx
            },
        );
        n.children = children;
        n
    }

    fn flex_col(children: Vec<BoxNode>, bx: BoxStyle) -> BoxNode {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Flex,
                dir: Dir::Column,
                ..bx
            },
        );
        n.children = children;
        n
    }

    #[test]
    fn min_height_flex_col_justify_end_pushes_content_down() {
        // hero: `min-h-[560px] flex-col justify-end` — a short stack is pushed
        // to the bottom of the 560px box (the NEXT outline floats above it)
        let mut root = flex_col(
            vec![leaf("pill")],
            BoxStyle {
                min_height: Some(Len::Px(560.0)),
                justify: Justify::End,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        assert_eq!(root.h, 560.0, "box floors to min-height");
        assert_eq!(l.runs.len(), 1);
        // text is pushed to the bottom of the 560px box (not left at the top)
        assert!(l.runs[0].y > 500.0, "y={} should be near the bottom", l.runs[0].y);
        assert!(l.runs[0].y < 560.0, "y={} within the box", l.runs[0].y);
    }

    #[test]
    fn nowrap_text_stays_on_one_line() {
        // `truncate` class (white-space:nowrap) must not wrap — the Ask AI
        // button stacked vertically without it
        let a = Atom {
            text: "Ask AI".into(),
            style: Style {
                ws: Ws::Nowrap,
                ..Style::new()
            },
            hard_break: false,
            link: None,
            node: None,
            cursor: None,
        };
        // width fits only ~2 chars → would wrap without nowrap
        let l = layout(&[a], 10.0, &|s, st| s.len() as f32 * st.size * 0.5);
        // words are separate runs; nowrap = they share one line (same y)
        assert!(l.runs.len() >= 1);
        let y0 = l.runs[0].y;
        assert!(
            l.runs.iter().all(|r| (r.y - y0).abs() < 0.01),
            "all words on one line"
        );
        assert!(l.height <= l.runs[0].style.line_height() + 0.01, "single line");
    }

    #[test]
    fn pct_width_child_measured_intrinsically_for_shrink_to_fit() {
        // `width:100%` inside an auto-width parent: max-content measurement
        // treats it as auto (cyclic against the very box it feeds), not as
        // filling the measuring context — when a flex row item contained a
        // width:100% descendant, its slot measured out as the whole row and
        // the sibling item was proportionally shrunk on top of it.
        let dom = crate::html::parse(
            r#"<div class="row"><div class="menu"><span class="p100">abcdefgh</span></div><div class="tool">Tools</div></div>"#,
        );
        let sheet = crate::css::parse(r#".row{display:flex}.p100{width:100%}"#);
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let l = layout_tree(&mut root, 200.0, &m4);
        let row = &root.children[0];
        let menu = &row.children[0];
        let tool = &row.children[1];
        // menu's slot = intrinsic "abcdefgh" (32px @ 4px/char), NOT row width
        assert!(menu.w < 60.0, "menu w={} measured intrinsically (<60), not 100% of row", menu.w);
        // no sibling overlap: Tools starts after the menu's full extent
        assert!(
            tool.x >= menu.x + menu.w - 0.01,
            "tool.x={} must not sit on the menu box (x={} w={})",
            tool.x,
            menu.x,
            menu.w
        );
        let tool_run = l.runs.iter().find(|r| r.text == "Tools").expect("Tools run");
        assert!(tool_run.x >= menu.x + menu.w, "tool run clear of the menu content");
    }

    #[test]
    fn flex_row_items_advance_by_their_margins() {
        // header-menu shape: `ul{display:flex}` with margin-bearing items
        // (e.g. `li{margin:0 8px}`) — each item's slot spans its margin box
        // so neighbors get real gaps (they used to lay edge to edge: the
        // whitespace between table-less list items is dropped in flex, and
        // the margins were measured/advanced as zero)
        let boxed = |t: &str| {
            let mut g = BoxNode::group(Style::new(), BoxStyle::default());
            g.children = vec![leaf(t)];
            g.bx.margin[3] = Len::Px(8.0);
            g.bx.margin[1] = Len::Px(8.0);
            g
        };
        let mut root = flex_row(vec![boxed("One"), boxed("Two")], BoxStyle::default());
        let l = layout_tree(&mut root, 500.0, &m4);
        let one = l.runs.iter().find(|r| r.text == "One").expect("One run");
        let two = l.runs.iter().find(|r| r.text == "Two").expect("Two run");
        let one_end = one.x + one.width;
        assert!(
            two.x >= one_end + 14.0,
            "real margin gap: two.x={} one ends at {} (want +16px margins minus slack)",
            two.x,
            one_end
        );
        assert!((one.y - two.y).abs() < 10.0, "same flex row");
    }

    #[test]
    fn min_height_shorter_content_unchanged() {        // content already taller than min-height → no shift, height stays
        // (boxed children: adjacent text leaves merge into one anonymous item)
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut root = flex_col(
            vec![boxed("a"), boxed("b"), boxed("c")],
            BoxStyle {
                min_height: Some(Len::Px(20.0)),
                justify: Justify::End,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        // 3 lines at default 15px × 1.35 line-height
        let content_h = 3.0 * 15.0 * 1.35;
        assert!((root.h - content_h).abs() < 0.01, "h={} content={}", root.h, content_h);
        assert_eq!(l.runs[0].y, 0.0, "first line stays at top");
    }

    #[test]
    fn flex_row_pct_width_item_keeps_sibling_on_the_row() {
        // A `width:100%` list beside a sibling in a justify-End row (the
        // wiki article-tools tabs + Tools menu): the list's slot is its %
        // basis capped at the space the sibling needs, so the sibling's
        // runs sit clear of the list's runs — never on top of them. Before
        // the cap the list reserved the whole row and justify-End slid its
        // runs over the sibling's spot.
        let tabs = flex_row(
            vec![leaf("Read"), leaf("View source"), leaf("View history")],
            BoxStyle {
                width: Some(Len::Pct(100.0)),
                justify: Justify::End,
                ..BoxStyle::default()
            },
        );
        let mut tool = BoxNode::group(Style::new(), BoxStyle::default());
        tool.children = vec![leaf("Tools")];
        let mut root = flex_row(vec![tabs, tool], BoxStyle::default());
        let l = layout_tree(&mut root, 200.0, &m4);
        let vh = l
            .runs
            .iter()
            .find(|r| r.text == "history")
            .expect("history run");
        let tools = l.runs.iter().find(|r| r.text == "Tools").expect("Tools run");
        assert!(
            tools.x >= vh.x + vh.width - 0.01,
            "Tools run (x={}) must sit right of the tab runs (end={})",
            tools.x,
            vh.x + vh.width
        );
        // the slot kept its intrinsic text too: tabs don't collapse to a sliver
        assert!(
            vh.x >= 100.0,
            "tabs justify-End inside their slot, last run x={} (too far left)",
            vh.x
        );
    }

    #[test]
    fn flex_row_pct_col_items_keep_their_row_share() {
        // two width:30% columns side by side: each slot is 30% OF THE ROW,
        // not 30% of the column's own intrinsic text width (that collapsed
        // each column to a sliver when the % was resolved slot-against-slot)
        let mut col_a = BoxNode::group(Style::new(), BoxStyle::default());
        col_a.bx.width = Some(Len::Pct(30.0));
        col_a.children = vec![leaf("aaaaaaaaaaaa")];
        let mut col_b = BoxNode::group(Style::new(), BoxStyle::default());
        col_b.bx.width = Some(Len::Pct(30.0));
        col_b.children = vec![leaf("bbbbbbbbbbbb")];
        let mut root = flex_row(vec![col_a, col_b], BoxStyle::default());
        layout_tree(&mut root, 200.0, &m4);
        let (a, b) = (&root.children[0], &root.children[1]);
        assert!((a.w - 60.0).abs() < 0.01, "col A w={} (want 60 = 30% of 200)", a.w);
        assert!((b.x - 60.0).abs() < 0.01, "col B x={} (want 60)", b.x);
        assert!((b.w - 60.0).abs() < 0.01, "col B w={}", b.w);
    }

    #[test]
    fn flex_row_places_items_with_gap() {
        // second item is a box: adjacent text leaves would merge into one
        // anonymous item (that's spec behavior, tested below)
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut root = flex_row(
            vec![boxed("ab"), boxed("cd")],
            BoxStyle {
                gap_col: 10.0,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        assert_eq!(l.runs.len(), 2);
        assert_eq!(l.runs[0].x, 0.0);
        assert_eq!(l.runs[1].x, 8.0 + 10.0, "gap between items");
        assert_eq!(l.runs[0].y, l.runs[1].y, "same row line");
    }

    #[test]
    fn adjacent_text_leaves_merge_into_one_item() {
        let mut root = flex_row(
            vec![leaf("ab"), leaf("cd")],
            BoxStyle {
                gap_col: 10.0,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        // one anonymous item: "ab cd" laid as a single text segment
        // (the space only advances x, it's not a run)
        assert_eq!(l.runs.len(), 2);
        assert_eq!(l.runs[0].x, 0.0);
        assert_eq!(l.runs[1].text, "cd");
        assert!(l.runs[1].x > l.runs[0].x, "space advanced x");
    }

    #[test]
    fn flex_row_shrinks_to_fit() {
        // two 40px items in a 60px box: shrink proportionally, no overflow
        let mut root = flex_row(
            vec![leaf("aaaaaaaaaa"), leaf("bbbbbbbbbb")],
            BoxStyle::default(),
        );
        let l = layout_tree(&mut root, 60.0, &m4);
        assert!(l.runs.iter().all(|r| r.x + r.width <= 60.0 + 0.001), "overflowed");
        assert_eq!(l.runs.len(), 2);
    }

    #[test]
    fn flex_row_wrap_breaks_lines() {
        // two 40px boxes in a 60px box with wrap: no shrink, second item
        // starts a new line (natural widths preserved); boxes, not leaves —
        // adjacent text leaves merge into one anonymous item
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut root = flex_row(
            vec![boxed("aaaaaaaaaa"), boxed("bbbbbbbbbb")],
            BoxStyle {
                wrap: true,
                gap_row: 5.0,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 60.0, &m4);
        assert_eq!(l.runs.len(), 2);
        assert_eq!(l.runs[0].width, 40.0, "natural width kept");
        assert_eq!(l.runs[0].y, l.runs[1].y - 15.0 * 1.35 - 5.0, "second line + gap");
        assert_eq!(l.runs[1].x, 0.0, "wrapped item starts at line start");
    }

    #[test]
    fn flex_row_wrap_keeps_fitting_items_together() {
        // three 20px items, 60px box: all fit one line (no wrap taken)
        let mut root = flex_row(
            vec![leaf("aaaaa"), leaf("bbbbb"), leaf("ccccc")],
            BoxStyle {
                wrap: true,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 60.0, &m4);
        let ys: std::collections::HashSet<i32> = l.runs.iter().map(|r| r.y as i32).collect();
        assert_eq!(ys.len(), 1, "single line when everything fits");
        assert!(l.runs.iter().all(|r| r.width == 20.0), "no shrink with wrap");
    }

    #[test]
    fn flex_row_justify_center() {
        let mut root = flex_row(
            vec![leaf("ab")],
            BoxStyle {
                justify: Justify::Center,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 100.0, &m4);
        // item is 8px wide, centered in 100 → x = 46
        assert!((l.runs[0].x - 46.0).abs() < 0.01, "x={}", l.runs[0].x);
    }

    #[test]
    fn flex_row_align_end_shifts_short_item_down() {
        // item 1: one text line; item 2: block box with 2 lines (via <br>)
        let tall = {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![
                leaf("x"),
                BoxNode::leaf(Style::new(), String::new(), true, None), // <br>
                leaf("y"),
            ];
            g
        };
        let mut root = flex_row(
            vec![leaf("aa"), tall],
            BoxStyle {
                align: Align::End,
                ..BoxStyle::default()
            },
        );
        let l = layout_tree(&mut root, 200.0, &m4);
        let line = 15.0 * 1.35;
        // <br> advances exactly one line (no paragraph gap) - the tall item
        // is two lines tall; the short item shifts to the row bottom
        let short = l.runs.iter().find(|r| r.text == "aa").unwrap();
        let tall_first = l.runs.iter().find(|r| r.text == "x").unwrap();
        assert!((short.y - (tall_first.y + line)).abs() < 0.01, "short.y={}", short.y);
        assert!((root.h - line * 2.0).abs() < 0.01, "h={}", root.h);
    }

    #[test]
    fn flex_col_stacks_with_row_gap() {
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Flex,
                dir: Dir::Column,
                gap_row: 10.0,
                ..BoxStyle::default()
            },
        );
        n.children = vec![boxed("a"), boxed("b")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert_eq!(l.runs.len(), 2);
        let line = 15.0 * 1.35;
        assert!((l.runs[1].y - (line + 10.0)).abs() < 0.01, "y={}", l.runs[1].y);
    }

    #[test]
    fn grid_three_equal_columns() {
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Grid,
                cols: vec![Track::Fr(1.0), Track::Fr(1.0), Track::Fr(1.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![boxed("a"), boxed("b"), boxed("c"), boxed("d")];
        let l = layout_tree(&mut n, 90.0, &m4);
        assert_eq!(l.runs.len(), 4);
        // row 1: x = 0, 30, 60; row 2 wraps to x = 0 on a new line
        assert_eq!(l.runs[0].x, 0.0);
        assert_eq!(l.runs[1].x, 30.0);
        assert_eq!(l.runs[2].x, 60.0);
        assert_eq!(l.runs[3].x, 0.0);
        let line = 15.0 * 1.35;
        assert!(l.runs[3].y >= line, "second row below first: {}", l.runs[3].y);
    }

    #[test]
    fn grid_min_content_tracks_size_to_items_and_stay_on_one_row() {
        let boxed = |t: &str| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            g.children = vec![leaf(t)];
            g
        };
        // MDN-header shape: `min-content 1fr min-content min-content`
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Grid,
                cols: vec![Track::Content, Track::Fr(1.0), Track::Content, Track::Content],
                ..BoxStyle::default()
            },
        );
        n.children = vec![boxed("AAAA"), boxed("B"), boxed("CCCCCC"), boxed("DD")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert_eq!(l.runs.len(), 4);
        // all four on ONE row — before min-content support this collapsed to a
        // single fr column and stacked them vertically
        let ys: Vec<f32> = l.runs.iter().map(|r| r.y).collect();
        let spread =
            ys.iter().cloned().fold(f32::MIN, f32::max) - ys.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 1.0, "one horizontal row, y spread={spread}");
        // content tracks size to their item's max-content width (4px/char):
        // col0 = "AAAA" = 16px, then the 1fr col absorbs the rest
        // (100 - 16 - 24 - 8 = 52), col2 = "CCCCCC" = 24px, col3 = "DD" = 8px
        let x = |t: &str| l.runs.iter().find(|r| r.text == t).unwrap().x;
        assert!((x("AAAA") - 0.0).abs() < 0.01, "col0 x={}", x("AAAA"));
        assert!((x("B") - 16.0).abs() < 0.01, "col1 (1fr) x={}", x("B"));
        assert!((x("CCCCCC") - 68.0).abs() < 0.01, "col2 x={}", x("CCCCCC"));
        assert!((x("DD") - 92.0).abs() < 0.01, "col3 x={}", x("DD"));
    }

    #[test]
    fn width_px_and_margin_auto_center() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                width: Some(Len::Px(50.0)),
                margin_x_auto: true,
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("hello world wrap")];
        let l = layout_tree(&mut n, 150.0, &m4);
        // box x = (150 - 50) / 2 = 50; first run starts at box x
        assert!((n.x - 50.0).abs() < 0.01, "box x={}", n.x);
        assert!((l.runs[0].x - 50.0).abs() < 0.01);
        assert!(n.w <= 50.0 + 0.001);
    }

    #[test]
    fn margin_left_auto_pushes_right_not_centers() {
        // `margin-left: auto` alone absorbs free space on the LEFT, so the
        // box hugs the right edge (the old shortcut centered it instead)
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                width: Some(Len::Px(50.0)),
                margin_l_auto: true,
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("push right")];
        let l = layout_tree(&mut n, 150.0, &m4);
        // box x = 150 - 50 = 100; NOT (150-50)/2 = 50
        assert!((n.x - 100.0).abs() < 0.01, "box x={}", n.x);
        assert!((l.runs[0].x - 100.0).abs() < 0.01);
    }

    // ---- box-sizing ----

    /// `width: 100px; padding: 10px; border: 1px solid` on a block with an
    /// auto-width block child (the child measures the content area)
    fn padded_box(content_box: bool) -> BoxNode {
        let mut inner = BoxNode::group(Style::new(), BoxStyle::default());
        inner.children = vec![leaf("hello")];
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                width: Some(Len::Px(100.0)),
                pad: [Len::Px(10.0); 4],
                border: [Len::Px(1.0); 4],
                content_box,
                ..BoxStyle::default()
            },
        );
        n.children = vec![inner];
        n
    }

    #[test]
    fn border_box_default_declared_width_includes_padding_border() {
        // regression guard: the default (and the CSS-initial-value shortcut
        // this engine uses) — a declared width IS the border box
        let mut n = padded_box(false);
        let _l = layout_tree(&mut n, 500.0, &m4);
        assert!((n.w - 100.0).abs() < 0.01, "border box w={}", n.w);
        // content area = 100 - 20 padding - 2 border
        assert!((n.children[0].w - 78.0).abs() < 0.01, "content w={}", n.children[0].w);
    }

    #[test]
    fn content_box_declared_width_measures_content_only() {
        let mut n = padded_box(true);
        let _l = layout_tree(&mut n, 500.0, &m4);
        // border box grows: 100 + 20 padding + 2 border = 122
        assert!((n.w - 122.0).abs() < 0.01, "border box w={}", n.w);
        // content width stays exactly the declared 100
        assert!((n.children[0].w - 100.0).abs() < 0.01, "content w={}", n.children[0].w);
    }

    #[test]
    fn content_box_with_auto_margins_still_centers() {
        let mut n = padded_box(true);
        n.bx.width = Some(Len::Px(50.0)); // like width_px_and_margin_auto_center
        n.bx.margin_x_auto = true;
        let l = layout_tree(&mut n, 150.0, &m4);
        // border box = 50 + 20 padding + 2 border = 72, centered in 150 →
        // x = 39 — centering must use the grown border box, not the 50px
        // declared width (which would put x at (150-50)/2 = 50)
        assert!((n.w - 72.0).abs() < 0.01, "border box w={}", n.w);
        assert!((n.x - 39.0).abs() < 0.01, "box x={}", n.x);
        // text starts at the content origin: x + border + padding = 50
        assert!((l.runs[0].x - 50.0).abs() < 0.01, "run x={}", l.runs[0].x);
    }

    #[test]
    fn block_flow_paragraph_gap() {
        // two block children: second starts after first's height + 0.6*size gap
        let mut n = BoxNode::group(Style::new(), BoxStyle::default());
        let mk_block = |t: &str| {
            let mut b = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    ..BoxStyle::default()
                },
            );
            b.children = vec![leaf(t)];
            b
        };
        n.children = vec![mk_block("one"), mk_block("two")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert_eq!(l.runs.len(), 2);
        let line = 15.0 * 1.35;
        let expect = line + 15.0 * 0.6;
        assert!((l.runs[1].y - expect).abs() < 0.01, "y={} expect={}", l.runs[1].y, expect);
    }

    #[test]
    fn text_align_center_shifts_line() {
        let mut n = BoxNode::group(Style::new(), BoxStyle::default());
        // text-align is inherited, so the leaf carries it (build_into does this)
        n.children = vec![BoxNode::leaf(
            Style {
                text_align: 1,
                ..Style::new()
            },
            "ab".into(),
            false,
            None,
        )];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert!((l.runs[0].x - 46.0).abs() < 0.01, "x={}", l.runs[0].x);
    }

    // ---- float / clear / vertical-align ----

    #[test]
    fn float_left_shortens_sibling_lines_and_takes_no_stack_space() {
        // infobox shape: `<img style="float:left"> + prose. The float sits
        // at the content top-left, the following BLOCK sibling starts at
        // y=0 (a float reserves no vertical stack space), and that
        // sibling's own text lines shorten around the float rect (floats
        // are threaded down through place() into descendant blocks).
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut fl = img_box(BoxStyle {
            float: Float::Left,
            ..BoxStyle::default()
        });
        fl.inline = true; // real <img> boxes are inline — float claims first
        let mut blk = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                ..BoxStyle::default()
            },
        );
        blk.children = vec![leaf(&"a ".repeat(150))];
        root.children = vec![fl, blk];
        let l = layout_tree(&mut root, 300.0, &m4);
        // float: intrinsic 100×50 at the content top-left
        let f = &root.children[0];
        assert!(
            (f.x - 0.0).abs() < 0.01 && (f.y - 0.0).abs() < 0.01,
            "float at ({}, {})",
            f.x,
            f.y
        );
        assert!((f.w - 100.0).abs() < 0.01 && (f.h - 50.0).abs() < 0.01, "float {}x{}", f.w, f.h);
        assert!(
            (root.children[1].y - 0.0).abs() < 0.01,
            "sibling y={} — the float takes no vertical space in the stack",
            root.children[1].y
        );
        // every line whose y-span meets the float's clears its rect …
        let lh = 15.0 * 1.35;
        let mut beside = 0;
        for r in &l.runs {
            if r.y < f.y + f.h && r.y + lh > f.y {
                beside += 1;
                assert!(
                    r.x >= f.x + f.w - 0.01,
                    "run y={} x={} overlaps the float rect",
                    r.y,
                    r.x
                );
            }
        }
        assert!(beside >= 3, "several lines run beside the float, got {}", beside);
        // …and a line below the float re-uses the full width (x back at 0)
        assert!(
            l.runs.iter().any(|r| r.y >= f.y + f.h - 0.01 && r.x < 1.0),
            "a line below the float starts back at x=0"
        );
    }

    #[test]
    fn float_right_sits_at_the_right_edge() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut fl = img_box(BoxStyle {
            float: Float::Right,
            ..BoxStyle::default()
        });
        fl.inline = true;
        let mut blk = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                ..BoxStyle::default()
            },
        );
        blk.children = vec![leaf(&"a ".repeat(150))];
        root.children = vec![fl, blk];
        let l = layout_tree(&mut root, 300.0, &m4);
        let f = &root.children[0];
        // hugs the right edge of the 300px content box
        assert!((f.x - 200.0).abs() < 0.01, "float x={} (expect 200)", f.x);
        assert!((f.y - 0.0).abs() < 0.01, "float y={}", f.y);
        assert!((f.w - 100.0).abs() < 0.01 && (f.h - 50.0).abs() < 0.01, "float {}x{}", f.w, f.h);
        // lines beside the float end before its left edge
        let lh = 15.0 * 1.35;
        for r in &l.runs {
            if r.y < f.y + f.h && r.y + lh > f.y {
                assert!(
                    r.x + r.width <= f.x + 0.01,
                    "run ends at {} — overlaps the right float at x={}",
                    r.x + r.width,
                    f.x
                );
            }
        }
    }

    #[test]
    fn clear_left_pushes_sibling_below_the_float() {
        // `<img float:left>` + `<div clear:left>`: the cleared sibling's
        // flow y jumps to the float's bottom (50) before it is placed
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut fl = img_box(BoxStyle {
            float: Float::Left,
            ..BoxStyle::default()
        });
        fl.inline = true;
        let mut blk = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                clear: Clear::Left,
                ..BoxStyle::default()
            },
        );
        blk.children = vec![leaf("abcde")];
        root.children = vec![fl, blk];
        let l = layout_tree(&mut root, 300.0, &m4);
        assert!(
            (root.children[1].y - 50.0).abs() < 0.01,
            "cleared sibling y={} (float bottom = 50)",
            root.children[1].y
        );
        assert!((l.runs[0].y - 50.0).abs() < 0.01, "its text starts at y={}", l.runs[0].y);
        assert!((l.runs[0].x - 0.0).abs() < 0.01, "below the float: full width");

        // clear:right against a LEFT float is a no-op — side-specific …
        let mut root2 = BoxNode::group(Style::new(), BoxStyle::default());
        let mut fl2 = img_box(BoxStyle {
            float: Float::Left,
            ..BoxStyle::default()
        });
        fl2.inline = true;
        let mut blk2 = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                clear: Clear::Right,
                ..BoxStyle::default()
            },
        );
        blk2.children = vec![leaf("abcde")];
        root2.children = vec![fl2, blk2];
        let l2 = layout_tree(&mut root2, 300.0, &m4);
        assert!(
            (root2.children[1].y - 0.0).abs() < 0.01,
            "clear:right must not push below a LEFT float (y={})",
            root2.children[1].y
        );
        // …but its lines still avoid the float it wasn't clearing
        assert!(
            l2.runs[0].x >= 100.0 - 0.01,
            "uncleared sibling's line still narrows around the float, x={}",
            l2.runs[0].x
        );
    }

    #[test]
    fn full_width_float_wraps_lines_below_it() {
        // zero room: the float spans the container, so the first line has
        // no usable x-range and drops to the float's bottom instead of
        // overlapping it
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut fl = img_box(BoxStyle {
            width: Some(Len::Px(300.0)),
            float: Float::Left,
            ..BoxStyle::default()
        });
        fl.inline = true;
        root.children = vec![fl, leaf("abcde")];
        let l = layout_tree(&mut root, 300.0, &m4);
        let f = &root.children[0];
        assert!((f.w - 300.0).abs() < 0.01 && (f.h - 150.0).abs() < 0.01, "float {}x{}", f.w, f.h);
        assert_eq!(l.runs.len(), 1);
        assert!(
            (l.runs[0].y - 150.0).abs() < 0.01,
            "line wrapped below the float, y={}",
            l.runs[0].y
        );
        assert!((l.runs[0].x - 0.0).abs() < 0.01, "x={}", l.runs[0].x);
    }

    #[test]
    fn vertical_align_middle_raises_inline_box_vs_baseline() {
        // an inline box AFTER a text line: baseline/absent keeps today's
        // line centering exactly; `middle` puts the box's midpoint at
        // baseline − x-height/2, i.e. higher on the same line
        let mk = |va: VAlign| {
            let mut n = BoxNode::group(Style::new(), BoxStyle::default());
            let mut ib = img_box(BoxStyle {
                width: Some(Len::Px(20.0)), // → 20×10 intrinsic aspect
                valign: va,
                ..BoxStyle::default()
            });
            ib.inline = true;
            n.children = vec![leaf("ab"), ib];
            n
        };
        let mut base = mk(VAlign::Baseline);
        let mut mid = mk(VAlign::Middle);
        layout_tree(&mut base, 300.0, &m4);
        layout_tree(&mut mid, 300.0, &m4);
        let line = 15.0 * 1.35;
        let base_y = base.children[1].y;
        let mid_y = mid.children[1].y;
        // default/baseline = the old centering: (line_h − box_h)/2
        assert!(
            (base_y - (line - 10.0) * 0.5).abs() < 0.01,
            "baseline y={} (expect {})",
            base_y,
            (line - 10.0) * 0.5
        );
        assert!(mid_y < base_y, "middle y={} should sit above baseline y={}", mid_y, base_y);
        // …and an explicit `baseline` matches the absent-property case
        let mut expl = mk(VAlign::Baseline);
        layout_tree(&mut expl, 300.0, &m4);
        assert!((expl.children[1].y - base_y).abs() < 0.01, "explicit baseline == default");
    }

    #[test]
    fn no_float_clear_valign_keeps_known_geometry() {
        // guard: a fixture using NONE of the three properties lays out at
        // the exact numbers the engine produced before they existed
        let mut n = BoxNode::group(Style::new(), BoxStyle::default());
        let mut b1 = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                ..BoxStyle::default()
            },
        );
        let mut ib = img_box(BoxStyle {
            width: Some(Len::Px(20.0)),
            ..BoxStyle::default()
        });
        ib.inline = true;
        b1.children = vec![leaf("one"), ib];
        let mut b2 = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                ..BoxStyle::default()
            },
        );
        b2.children = vec![leaf("two")];
        n.children = vec![b1, b2];
        let l = layout_tree(&mut n, 100.0, &m4);
        let line = 15.0 * 1.35;
        // b1: one text line with the inline box centered in it
        assert!((n.children[0].h - line).abs() < 0.01, "b1.h={}", n.children[0].h);
        assert!(
            (n.children[0].children[1].y - (line - 10.0) * 0.5).abs() < 0.01,
            "inline box y={}",
            n.children[0].children[1].y
        );
        // b2 starts after b1 + the 0.6em paragraph gap — same geometry as
        // the pre-float engine
        let expect = line + 15.0 * 0.6;
        assert!((n.children[1].y - expect).abs() < 0.01, "b2.y={} expect={}", n.children[1].y, expect);
        assert_eq!(l.runs.len(), 2);
        assert!((l.runs[1].y - expect).abs() < 0.01, "second run y={}", l.runs[1].y);
    }

    #[test]
    fn floated_inline_span_gets_a_box_and_flows_as_a_float() {
        // end-to-end: parse → build → layout. A floated <span> blockifies
        // into its own box (it would otherwise flatten into the parent's
        // text and lose the float), sits at the flow edge, and the text
        // after it shortens around the rect.
        let dom = crate::html::parse(r#"<p><span style="float:left">side</span>body</p>"#);
        let sheet = crate::css::parse("");
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let l = layout_tree(&mut root, 100.0, &m4);
        let p = &root.children[0];
        let span = &p.children[0];
        assert_eq!(span.bx.float, Float::Left, "float parsed through the pipeline");
        assert!(!span.inline, "floated inline blockifies to a block-level box");
        assert!(
            (span.x - 0.0).abs() < 0.01 && (span.y - 0.0).abs() < 0.01,
            "float at ({}, {})",
            span.x,
            span.y
        );
        let side = l.runs.iter().find(|r| r.text == "side").expect("side run in the float");
        let body = l.runs.iter().find(|r| r.text == "body").expect("body run");
        assert!((body.y - side.y).abs() < 0.01, "both on the first line");
        assert!(
            body.x >= span.x + span.w - 0.01,
            "body x={} must clear the float (w={})",
            body.x,
            span.w
        );
    }

    // ---- box chrome (padding/margin/line-height/background) ----

    #[test]
    fn padding_insets_content_and_grows_box() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                pad: [Len::Px(10.0), Len::Px(20.0), Len::Px(10.0), Len::Px(20.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("ab")];
        let l = layout_tree(&mut n, 200.0, &m4);
        // content starts at left padding; box height = line + top + bottom pad
        assert!((l.runs[0].x - 20.0).abs() < 0.01, "x={}", l.runs[0].x);
        assert!((l.runs[0].y - 10.0).abs() < 0.01, "y={}", l.runs[0].y);
        assert!((n.h - (15.0 * 1.35 + 20.0)).abs() < 0.01, "h={}", n.h);
        // border-box width unchanged
        assert!((n.w - 200.0).abs() < 0.01);
    }

    #[test]
    fn padding_shrinks_wrap_width() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                pad: [Len::Px(0.0), Len::Px(25.0), Len::Px(0.0), Len::Px(25.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("aaaa bbbb cccc")];
        let l = layout_tree(&mut n, 100.0, &m4);
        // content width = 50px → "aaaa"(16)+"bbbb"(16)+"cccc"(16) wraps to 2+ lines
        let ys: std::collections::HashSet<i32> = l.runs.iter().map(|r| r.y as i32).collect();
        assert!(ys.len() >= 2, "expected wrap in 50px content, got {} lines", ys.len());
        for r in &l.runs {
            assert!(r.x <= 75.0 + 0.001, "run escapes right padding: {:?}", r.text);
        }
    }

    #[test]
    fn border_insets_content_and_grows_box() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                border: [Len::Px(2.0), Len::Px(4.0), Len::Px(2.0), Len::Px(4.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("ab")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert!((l.runs[0].x - 4.0).abs() < 0.01, "x={}", l.runs[0].x);
        assert!((l.runs[0].y - 2.0).abs() < 0.01, "y={}", l.runs[0].y);
        assert!((n.h - (15.0 * 1.35 + 4.0)).abs() < 0.01, "h={}", n.h);
        assert!((n.w - 100.0).abs() < 0.01, "border-box width unchanged");
    }

    #[test]
    fn border_paints_strips_or_ring() {
        // square box with bg: background + 4 side strips
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                border: [Len::Px(1.0); 4],
                bg: Some(0x123456),
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("a")];
        let l = layout_tree(&mut n, 50.0, &m4);
        assert_eq!(l.rects.len(), 5, "bg + 4 strips: {:?}", l.rects);
        assert_eq!(l.rects[1].4, 0x000000, "unspecified border = currentcolor");
        // rounded box with bg: ring = border-box fill + background inset over it
        let mut m2 = BoxNode::group(
            Style::new(),
            BoxStyle {
                border: [Len::Px(2.0); 4],
                radius: Len::Px(6.0),
                bg: Some(0x123456),
                ..BoxStyle::default()
            },
        );
        m2.children = vec![leaf("a")];
        let l2 = layout_tree(&mut m2, 50.0, &m4);
        assert_eq!(l2.rects.len(), 2, "ring = outer + inner: {:?}", l2.rects);
        assert_eq!(l2.rects[0].4, 0x000000, "outer ring = currentcolor (text color)");
        assert!((l2.rects[1].2 - 46.0).abs() < 0.01, "inner w={}", l2.rects[1].2);
        assert!((l2.rects[1].5 - 4.0).abs() < 0.01, "inner radius={}", l2.rects[1].5);
    }

    #[test]
    fn shadow_emitted_offset_spread_alpha_fixed() {
        use crate::css::Shadow;
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                bg: Some(0xffffff),
                width: Some(Len::Px(100.0)),
                shadows: vec![Shadow {
                    dx: 0.0,
                    dy: 4.0,
                    blur: 10.0,
                    spread: 2.0,
                    color: Some(0x000000),
                    alpha: 128,
                }],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("a")];
        let l = layout_tree(&mut n, 200.0, &m4);
        assert_eq!(l.shadows.len(), 1, "one shadow rect");
        let s = &l.shadows[0];
        // spread grows the rect by 2px each side, offset down 4px
        assert!((s.x - -2.0).abs() < 0.01, "x={}", s.x);
        assert!((s.y - 2.0).abs() < 0.01, "y={}", s.y);
        assert!((s.w - 104.0).abs() < 0.01, "w={}", s.w);
        // plain blocks size to content (declared height only applies to
        // replaced elements) — compare against the emitted bg rect
        assert!((s.h - (l.rects[0].3 + 4.0)).abs() < 0.01, "h={}", s.h);
        assert_eq!(s.blur, 10.0);
        assert_eq!(s.color, (128u32 << 24) | 0x000000, "alpha packed in");
        assert!(!s.fixed, "in-flow box");
        // currentcolor: no shadow color → the element's text color
        let mut m2 = BoxNode::group(
            Style::new(),
            BoxStyle {
                shadows: vec![Shadow {
                    dx: 1.0,
                    dy: 1.0,
                    blur: 0.0,
                    spread: 0.0,
                    color: None,
                    alpha: 255,
                }],
                pos: Pos::Fixed,
                ..BoxStyle::default()
            },
        );
        m2.children = vec![leaf("a")];
        let l2 = layout_tree(&mut m2, 200.0, &m4);
        assert_eq!(l2.shadows[0].color, 0xff000000, "currentcolor (alpha-premultiplied)");
        assert!(l2.shadows[0].fixed, "fixed subtree");
    }

    #[test]
    fn margin_top_advances_sibling() {
        let mk = |mt: Len| {
            let mut b = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    margin: [mt, Len::Px(0.0), Len::Px(0.0), Len::Px(0.0)],
                    ..BoxStyle::default()
                },
            );
            b.children = vec![leaf("x")];
            b
        };
        let mut n = BoxNode::group(Style::new(), BoxStyle::default());
        n.children = vec![mk(Len::Px(0.0)), mk(Len::Px(15.0))];
        let l = layout_tree(&mut n, 100.0, &m4);
        let line = 15.0 * 1.35;
        let gap = 15.0 * 0.6; // block-boundary paragraph gap
        // second block: first line + gap + 15px margin-top
        assert!(
            (l.runs[1].y - (line + gap + 15.0)).abs() < 0.01,
            "y={}",
            l.runs[1].y
        );
    }

    #[test]
    fn line_height_overrides_default() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle::default(),
        );
        n.children = vec![BoxNode::leaf(
            Style {
                lh: 30.0,
                ..Style::new()
            },
            "a b".into(),
            false,
            None,
        )];
        let l = layout_tree(&mut n, 8.0, &m4); // 4px words, wraps to 2 lines
        let ys: Vec<f32> = {
            let mut v: Vec<f32> = l.runs.iter().map(|r| r.y).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v.dedup();
            v
        };
        assert_eq!(ys.len(), 2, "expected 2 lines, runs={}", l.runs.len());
        assert!((ys[1] - ys[0] - 30.0).abs() < 0.01, "line delta={}", ys[1] - ys[0]);
    }

    #[test]
    fn background_emits_rect_under_content() {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                bg: Some(0x112233),
                radius: Len::Px(8.0),
                pad: [Len::Px(5.0), Len::Px(5.0), Len::Px(5.0), Len::Px(5.0)],
                ..BoxStyle::default()
            },
        );
        n.children = vec![leaf("hi")];
        let l = layout_tree(&mut n, 100.0, &m4);
        assert_eq!(l.rects.len(), 1);
        let &(x, y, w, h, color, rad, fixed) = &l.rects[0];
        assert!(!fixed, "static box is not fixed");
        assert_eq!(color, 0x112233);
        assert!((x - 0.0).abs() < 0.01 && (y - 0.0).abs() < 0.01);
        assert!((w - 100.0).abs() < 0.01, "w={}", w);
        assert!((h - (15.0 * 1.35 + 10.0)).abs() < 0.01, "h={}", h);
        assert!((rad - 8.0).abs() < 0.01, "radius={}", rad);
    }

    // ---- replaced elements (<img>) ----

    /// group box carrying a decoded 100×50 image.
    fn img_box(bx: BoxStyle) -> BoxNode {
        let mut n = BoxNode::group(Style::new(), bx);
        n.img = Some(ImgSlot::Ready(std::rc::Rc::new(crate::image::ImageData {
            w: 100,
            h: 50,
            px: vec![0; 100 * 50],
        })));
        n
    }

    /// group box carrying a decoded image of the given intrinsic size.
    fn img_sized(bx: BoxStyle, w: u32, h: u32) -> BoxNode {
        let mut n = BoxNode::group(Style::new(), bx);
        n.img = Some(ImgSlot::Ready(std::rc::Rc::new(crate::image::ImageData {
            w,
            h,
            px: vec![0; (w * h) as usize],
        })));
        n
    }

    #[test]
    fn img_uses_intrinsic_size_clamped_to_avail() {
        let mut n = img_box(BoxStyle::default());
        let l = layout_tree(&mut n, 300.0, &m4);
        assert!((n.w - 100.0).abs() < 0.01, "w={}", n.w);
        assert!((n.h - 50.0).abs() < 0.01, "h={}", n.h);
        assert_eq!(l.images.len(), 1);
        assert!((l.images[0].3 - 50.0).abs() < 0.01);

        let mut narrow = img_box(BoxStyle::default());
        layout_tree(&mut narrow, 60.0, &m4);
        assert!((narrow.w - 60.0).abs() < 0.01, "clamped w={}", narrow.w);
        assert!((narrow.h - 30.0).abs() < 0.01, "aspect h={}", narrow.h);
    }

    #[test]
    fn img_width_declared_height_keeps_aspect() {
        let mut n = img_box(BoxStyle {
            width: Some(Len::Px(50.0)),
            ..BoxStyle::default()
        });
        layout_tree(&mut n, 300.0, &m4);
        assert!((n.w - 50.0).abs() < 0.01, "w={}", n.w);
        assert!((n.h - 25.0).abs() < 0.01, "h={}", n.h);
    }

    #[test]
    fn img_declared_height_wins_over_aspect() {
        let mut n = img_box(BoxStyle {
            height: Some(Len::Px(80.0)),
            ..BoxStyle::default()
        });
        layout_tree(&mut n, 300.0, &m4);
        assert!((n.h - 80.0).abs() < 0.01, "h={}", n.h);
    }

    // ---- object-fit / object-position / aspect-ratio ----

    #[test]
    fn object_fit_content_rect_for_8x6_in_100x100() {
        let bx = (0.0, 0.0, 100.0, 100.0);
        let c = [BgPos::Pct(50.0); 2];
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;

        // fill = exact stretch (today's behavior)
        let (r, clip) = fit_content_rect(ObjFit::Fill, c, bx, 8.0, 6.0);
        assert_eq!(r, bx);
        assert!(clip.is_none());

        // contain = letterbox: 100×75 at y = 12.5
        let (r, clip) = fit_content_rect(ObjFit::Contain, c, bx, 8.0, 6.0);
        assert!(near(r.2, 100.0) && near(r.3, 75.0), "r={r:?}");
        assert!(near(r.1, 12.5), "centered y={}", r.1);
        assert!(clip.is_none(), "contained content does not crop");

        // cover = crop: 133.3×100 at x = −16.67, clipped to the box
        let (r, clip) = fit_content_rect(ObjFit::Cover, c, bx, 8.0, 6.0);
        assert!(near(r.0, -16.6667) && near(r.2, 133.3333), "r={r:?}");
        assert!(near(r.3, 100.0), "r={r:?}");
        assert_eq!(clip, Some((0.0, 0.0, 100.0, 100.0)));

        // none = natural size, centered
        let (r, _) = fit_content_rect(ObjFit::None, c, bx, 8.0, 6.0);
        assert!(near(r.0, 46.0) && near(r.1, 47.0), "r={r:?}");
        assert_eq!((r.2, r.3), (8.0, 6.0));

        // scale-down = the smaller of none and contain
        let (r, _) = fit_content_rect(ObjFit::ScaleDown, c, bx, 10.0, 10.0);
        assert_eq!((r.2, r.3), (10.0, 10.0), "small image stays natural");
        let (r, _) = fit_content_rect(ObjFit::ScaleDown, c, bx, 200.0, 200.0);
        assert_eq!((r.2, r.3), (100.0, 100.0), "large image contains");
    }

    #[test]
    fn object_position_anchors_the_contained_rect() {
        let bx = (0.0, 0.0, 100.0, 100.0);
        // 0% 0% pins the letterbox to the top-left
        let (r, _) = fit_content_rect(
            ObjFit::Contain,
            [BgPos::Pct(0.0), BgPos::Pct(0.0)],
            bx,
            8.0,
            6.0,
        );
        assert_eq!((r.0, r.1), (0.0, 0.0));
        // px lengths offset from the box edge
        let (r, _) = fit_content_rect(
            ObjFit::Contain,
            [BgPos::Px(10.0), BgPos::Px(20.0)],
            bx,
            8.0,
            6.0,
        );
        assert_eq!((r.0, r.1), (10.0, 20.0));
    }

    #[test]
    fn object_fit_place_sets_content_rect_and_clip() {
        let mut n = img_sized(
            BoxStyle {
                width: Some(Len::Px(100.0)),
                height: Some(Len::Px(100.0)),
                obj_fit: ObjFit::Contain,
                ..BoxStyle::default()
            },
            8,
            6,
        );
        layout_tree(&mut n, 300.0, &m4);
        let r = n.img_fit.expect("contain computes a content rect");
        assert!((r.1 - 12.5).abs() < 0.01 && (r.3 - 75.0).abs() < 0.01, "r={r:?}");
        assert!(n.img_clip.is_none());
        // the paint entry follows the content rect (letterboxed, not full box)
        let img = &layout_tree(&mut n, 300.0, &m4).images;
        assert_eq!(img.len(), 1);
        assert!((img[0].1 - 12.5).abs() < 0.01 && (img[0].3 - 75.0).abs() < 0.01);

        let mut c = img_sized(
            BoxStyle {
                width: Some(Len::Px(100.0)),
                height: Some(Len::Px(100.0)),
                obj_fit: ObjFit::Cover,
                ..BoxStyle::default()
            },
            8,
            6,
        );
        layout_tree(&mut c, 300.0, &m4);
        let r = c.img_fit.expect("cover computes a content rect");
        assert!((r.0 + 16.6667).abs() < 0.01 && (r.2 - 133.3333).abs() < 0.01, "r={r:?}");
        assert_eq!(c.img_clip, Some((0.0, 0.0, 100.0, 100.0)));
        let cov = &layout_tree(&mut c, 300.0, &m4).images;
        assert_eq!(cov.len(), 1);
        assert_eq!(
            (cov[0].0, cov[0].1, cov[0].2, cov[0].3),
            (0.0, 0.0, 100.0, 100.0),
            "cover draws only the clipped window"
        );

        // no object-fit declaration → legacy box draw, byte-identical
        let mut plain = img_sized(BoxStyle::default(), 8, 6);
        layout_tree(&mut plain, 300.0, &m4);
        assert!(plain.img_fit.is_none() && plain.img_clip.is_none());
        assert_eq!((plain.w, plain.h), (8.0, 6.0));
    }

    #[test]
    fn aspect_ratio_derives_the_missing_axis() {
        // <img> width + ratio → height
        let mut a = img_sized(
            BoxStyle {
                width: Some(Len::Px(320.0)),
                aspect_ratio: Some(16.0 / 9.0),
                ..BoxStyle::default()
            },
            8,
            6,
        );
        layout_tree(&mut a, 600.0, &m4);
        assert!((a.w - 320.0).abs() < 0.01 && (a.h - 180.0).abs() < 0.01, "a={}x{}", a.w, a.h);

        // <img> height + ratio → width
        let mut b = img_sized(
            BoxStyle {
                height: Some(Len::Px(50.0)),
                aspect_ratio: Some(2.0),
                ..BoxStyle::default()
            },
            8,
            6,
        );
        layout_tree(&mut b, 600.0, &m4);
        assert!((b.w - 100.0).abs() < 0.01 && (b.h - 50.0).abs() < 0.01, "b={}x{}", b.w, b.h);

        // plain boxes derive the same way (content laid out inside)
        let mut c = BoxNode::group(
            Style::new(),
            BoxStyle {
                width: Some(Len::Px(320.0)),
                aspect_ratio: Some(16.0 / 9.0),
                ..BoxStyle::default()
            },
        );
        layout_tree(&mut c, 600.0, &m4);
        assert!((c.w - 320.0).abs() < 0.01 && (c.h - 180.0).abs() < 0.01, "c={}x{}", c.w, c.h);

        let mut d = BoxNode::group(
            Style::new(),
            BoxStyle {
                height: Some(Len::Px(50.0)),
                aspect_ratio: Some(2.0),
                ..BoxStyle::default()
            },
        );
        layout_tree(&mut d, 600.0, &m4);
        assert!((d.w - 100.0).abs() < 0.01 && (d.h - 50.0).abs() < 0.01, "d={}x{}", d.w, d.h);

        // auto (None) keeps the intrinsic ratio
        let mut e = img_sized(
            BoxStyle {
                width: Some(Len::Px(320.0)),
                ..BoxStyle::default()
            },
            8,
            6,
        );
        layout_tree(&mut e, 600.0, &m4);
        assert!((e.h - 240.0).abs() < 0.01, "e.h={}", e.h);
    }

    #[test]
    fn inline_svg_becomes_img_box_with_raw_markup() {
        // <svg> is RAW_TEXT in the parser; build() must reassemble the full
        // markup into an ImgSlot so the image pipeline rasterizes it
        let dom = crate::html::parse(
            r#"<body><svg viewBox="0 0 10 20" width="10" height="20"><path d="M0 0h10"/></svg></body>"#,
        );
        let root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &crate::css::parse(""));
        fn find_svg(n: &BoxNode) -> Option<&ImgSlot> {
            if n.img.is_some() {
                return n.img.as_ref();
            }
            n.children.iter().find_map(find_svg)
        }
        let Some(ImgSlot::Pending(src)) = find_svg(&root) else {
            panic!("svg should produce a pending img slot");
        };
        assert!(src.starts_with("<svg"), "markup starts with root tag: {src:?}");
        // the HTML parser lowercases attr keys; svg.rs get_attr is
        // case-insensitive to match
        assert!(src.contains("viewbox=\"0 0 10 20\""), "attrs kept: {src:?}");
        assert!(src.contains("<path d=\"M0 0h10\"/>"), "inner markup kept: {src:?}");
        assert!(src.ends_with("</svg>"), "closed: {src:?}");
    }

    #[test]
    fn svg_class_fill_is_injected_as_inline_style() {
        // a site colors this SVG's children by class rule (.logo__letter {
        // fill: … }); build() must bake it into the path's inline style so the
        // cascade-less rasterizer paints it (the MDN-logo case)
        let dom = crate::html::parse(
            r#"<body><svg viewBox="0 0 10 10"><path class="logo__letter" d="M0 0h10"/></svg></body>"#,
        );
        let sheet = crate::css::parse(r#".logo__letter { fill: #0a5dbd }"#);
        let root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        fn find_svg(n: &BoxNode) -> Option<&ImgSlot> {
            if n.img.is_some() {
                return n.img.as_ref();
            }
            n.children.iter().find_map(find_svg)
        }
        let Some(ImgSlot::Pending(src)) = find_svg(&root) else {
            panic!("svg should produce a pending img slot");
        };
        assert!(
            src.contains("fill:#0a5dbd"),
            "class fill injected as inline style: {src:?}"
        );
    }

    #[test]
    fn svg_without_matching_class_rule_is_unchanged() {
        // no single-class fill rule → markup passes through byte-identical
        // (the common case; nextjs/rust-lang hero SVGs stay untouched)
        let dom = crate::html::parse(
            r#"<body><svg viewBox="0 0 10 10"><path class="other" d="M0 0h10"/></svg></body>"#,
        );
        let sheet = crate::css::parse(r#".logo__letter { fill: #0a5dbd }"#);
        let root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        fn find_svg(n: &BoxNode) -> Option<&ImgSlot> {
            if n.img.is_some() {
                return n.img.as_ref();
            }
            n.children.iter().find_map(find_svg)
        }
        let Some(ImgSlot::Pending(src)) = find_svg(&root) else {
            panic!("svg should produce a pending img slot");
        };
        assert!(!src.contains("fill:"), "no injection: {src:?}");
    }

    #[test]
    fn inject_tag_paint_merges_into_existing_style_and_skips_declared() {
        let sheet = crate::css::parse(
            r#".a { fill: #111111 } .b { fill: #222222; stroke: #333333 }"#,
        );
        // merges into an existing style="…" (semicolon-joined)
        let out = inject_tag_paint("<path class=\"a\" style=\"opacity:.5\" d=\"x\"/>", "a", &sheet, false);
        assert!(out.contains("opacity:.5;fill:#111111"), "merged: {out:?}");
        // a tag already declaring fill keeps its own value (no override)
        let out = inject_tag_paint("<path class=\"a\" fill=\"#ff0000\" d=\"x\"/>", "a", &sheet, false);
        assert!(!out.contains("fill:#111111"), "declared fill wins: {out:?}");
        // adds a style attribute when none exists, before the self-closing /
        let out = inject_tag_paint("<path class=\"b\" d=\"x\"/>", "b", &sheet, false);
        assert!(out.contains("style=\""), "style added: {out:?}");
        assert!(out.contains("fill:#222222") && out.contains("stroke:#333333"), "both props: {out:?}");
    }

    #[test]
    fn undecoded_img_with_declared_size_reserves_space() {
        // fetch/decode failed but width/height attributes were parsed → box still there
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                width: Some(Len::Px(40.0)),
                height: Some(Len::Px(20.0)),
                ..BoxStyle::default()
            },
        );
        n.img = Some(ImgSlot::Pending(std::rc::Rc::from("x.png")));
        let l = layout_tree(&mut n, 300.0, &m4);
        assert!((n.w - 40.0).abs() < 0.01);
        assert!((n.h - 20.0).abs() < 0.01, "declared h={}", n.h);
        assert!(l.images.is_empty(), "no pixels → nothing drawn");
    }

    #[test]
    fn img_advances_block_flow() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut first = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                ..BoxStyle::default()
            },
        );
        first.children = vec![leaf("ab")];
        let second = img_box(BoxStyle::default());
        root.children = vec![first, second];
        let l = layout_tree(&mut root, 300.0, &m4);
        let line = 15.0 * 1.35;
        let gap = 15.0 * 0.6;
        assert!((l.images[0].1 - (line + gap)).abs() < 0.01, "img y={}", l.images[0].1);
    }

    // ---- position (relative / absolute / fixed) ----

    /// group with `position` + insets, one text child.
    fn pos_box(pos: Pos, inset: [Option<Len>; 4], children: Vec<BoxNode>) -> BoxNode {
        let mut n = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                pos,
                inset,
                ..BoxStyle::default()
            },
        );
        n.children = children;
        n
    }

    #[test]
    fn absolute_leaves_flow_and_honors_insets() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let flow = pos_box(Pos::Static, [None; 4], vec![leaf("flowtxt")]);
        // top: 10, left: 20 against the root's padding box (= viewport here)
        let abs = pos_box(Pos::Absolute, [Some(Len::Px(10.0)), None, None, Some(Len::Px(20.0))], vec![leaf("abs")]);
        root.children = vec![flow, abs];
        let l = layout_tree(&mut root, 300.0, &m4);
        // out-of-flow: contributes nothing to the parent's height
        // (flow line + trailing paragraph gap after the child's open line)
        let flow_h = 15.0 * 1.35 + 15.0 * 0.6;
        assert!((root.h - flow_h).abs() < 0.01, "root.h={} (flow only)", root.h);
        let abs_run = l.runs.iter().find(|r| r.text == "abs").expect("abs run");
        let flow_run = l.runs.iter().find(|r| r.text == "flowtxt").expect("flow run");
        assert!((abs_run.x - 20.0).abs() < 0.01, "x={}", abs_run.x);
        assert!((abs_run.y - 10.0).abs() < 0.01, "y={}", abs_run.y);
        assert!(!abs_run.fixed);
        // flow run untouched by the absolute sibling
        assert!((flow_run.y - flow_h).abs() < 0.01 || flow_run.y == 0.0, "y={}", flow_run.y);
    }

    #[test]
    fn absolute_shrink_to_fit_and_right_bottom_insets() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        // right: 10, bottom: 10, no width → shrink-to-fit (4 chars × 4px)
        let abs = pos_box(
            Pos::Absolute,
            [None, Some(Len::Px(10.0)), Some(Len::Px(10.0)), None],
            vec![leaf("abcd")],
        );
        root.children = vec![abs];
        // layout_in with a 200×100 viewport
        let l = layout_in(&mut root, 300.0, 100.0, &m4);
        let run = l.runs.iter().find(|r| r.text == "abcd").expect("run");
        let bw = 16.0; // 4 chars × 4px
        let bh = 15.0 * 1.35;
        assert!((run.x - (300.0 - 10.0 - bw)).abs() < 0.01, "x={}", run.x);
        assert!((run.y - (100.0 - 10.0 - bh)).abs() < 0.01, "y={}", run.y);
        let abs_n = &root.children[0];
        assert!((abs_n.w - bw).abs() < 0.01, "shrink w={}", abs_n.w);
    }

    #[test]
    fn relative_shifts_subtree_and_runs() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut inner = pos_box(Pos::Static, [None; 4], vec![leaf("deep")]);
        inner.children = vec![leaf("deep")];
        let rel = pos_box(Pos::Relative, [Some(Len::Px(5.0)), None, None, Some(Len::Px(7.0))], vec![leaf("moved")]);
        root.children = vec![rel, inner];
        let l = layout_tree(&mut root, 300.0, &m4);
        // relative stays in flow: the sibling still starts at flow position
        // (two lines + a trailing paragraph gap after each open child line)
        let line = 15.0 * 1.35;
        let h = line * 2.0 + 15.0 * 0.6 * 2.0;
        assert!((root.h - h).abs() < 0.01, "h={} = two lines + gaps", root.h);
        let moved = l.runs.iter().find(|r| r.text == "moved").expect("run");
        assert!((moved.x - 7.0).abs() < 0.01, "x={}", moved.x);
        assert!((moved.y - 5.0).abs() < 0.01, "y={}", moved.y);
    }

    #[test]
    fn fixed_marks_paint_and_uses_viewport() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        // bottom bar: bottom: 0 against the viewport (layout_in vh = 100)
        let bar = pos_box(Pos::Fixed, [None, None, Some(Len::Px(0.0)), None], vec![leaf("bar")]);
        root.children = vec![bar];
        let l = layout_in(&mut root, 300.0, 100.0, &m4);
        let run = l.runs.iter().find(|r| r.text == "bar").expect("run");
        assert!(run.fixed, "run inside fixed subtree is marked fixed");
        let bh = 15.0 * 1.35;
        assert!((run.y - (100.0 - bh)).abs() < 0.01, "y={} = vh - h", run.y);
    }

    #[test]
    fn fixed_stretches_to_viewport_and_rect_marked_fixed() {
        // fixed with left+right insets stretches to the viewport width,
        // not the parent's — parent is a narrow positioned box
        let mut root = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                pos: Pos::Absolute,
                width: Some(Len::Px(150.0)),
                inset: [Some(Len::Px(50.0)), None, None, Some(Len::Px(50.0))],
                ..BoxStyle::default()
            },
        );
        root.children = vec![pos_box(
            Pos::Fixed,
            [Some(Len::Px(0.0)), Some(Len::Px(0.0)), None, Some(Len::Px(0.0))],
            vec![],
        )];
        root.children[0].bx.bg = Some(0x334455);
        let l = layout_in(&mut root, 300.0, 100.0, &m4);
        assert_eq!(l.rects.len(), 1, "bg → one rect: {:?}", l.rects);
        assert!(l.rects[0].6, "fixed box's rect carries the fixed flag");
        let fixed_box = &root.children[0];
        // parent: width 150 shifted to x=50; fixed ignores it — viewport CB
        assert!((root.w - 150.0).abs() < 0.01, "root w={}", root.w);
        assert!((root.x - 50.0).abs() < 0.01, "root x={}", root.x);
        assert!((fixed_box.w - 300.0).abs() < 0.01, "fixed w={} = viewport", fixed_box.w);
        assert!((fixed_box.x - 0.0).abs() < 0.01, "fixed x={} = viewport", fixed_box.x);
    }

    #[test]
    fn absolute_negative_top_left_lay_off_canvas() {
        // classic off-screen helper: top:-9999px must position the box
        // entirely above the canvas — nothing clamps it back into view
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let flow = pos_box(Pos::Static, [None; 4], vec![leaf("visible")]);
        let hidden = pos_box(
            Pos::Absolute,
            [Some(Len::Px(-9999.0)), None, None, Some(Len::Px(-9999.0))],
            vec![leaf("skip")],
        );
        root.children = vec![flow, hidden];
        let l = layout_in(&mut root, 300.0, 100.0, &m4);
        let run = l.runs.iter().find(|r| r.text == "skip").expect("skip run");
        let total_h = 15.0 * 1.35;
        assert!(
            run.y + total_h <= 0.0,
            "top:-9999px lays off-canvas: run.y={} (h={total_h})",
            run.y
        );
        assert!(run.x + run.width <= 0.0, "left:-9999px too: run.x={}", run.x);
        // no clamp to ≥0 anywhere: assert the raw magnitude survived
        assert!(run.y < -9000.0, "y={}", run.y);
        // the flow sibling is unaffected
        let vis = l.runs.iter().find(|r| r.text == "visible").expect("flow run");
        assert!(vis.y >= 0.0 && vis.y < 50.0, "in-flow text stays on canvas: y={}", vis.y);
    }

    // ---- position: sticky (in flow like static, pinned at paint time) ----

    #[test]
    fn sticky_keeps_static_flow_position_even_with_top() {
        // same tree three ways: a sticky box declaring top:40px must lay
        // out EXACTLY like static (the threshold is scroll-dependent and
        // never offsets flow — the anti-relative guard; relative WOULD
        // shift), and its following sibling must stack the same way.
        let build = |pos: Pos| {
            let mut root = BoxNode::group(Style::new(), BoxStyle::default());
            let head = pos_box(pos, [Some(Len::Px(40.0)), None, None, None], vec![leaf("head")]);
            let after = pos_box(Pos::Static, [None; 4], vec![leaf("after")]);
            root.children = vec![head, after];
            let l = layout_tree(&mut root, 300.0, &m4);
            let runs = l
                .runs
                .iter()
                .map(|r| (r.text.clone(), r.x, r.y))
                .collect::<Vec<_>>();
            (runs, root.h)
        };
        let (static_runs, static_h) = build(Pos::Static);
        let (sticky_runs, sticky_h) = build(Pos::Sticky);
        assert_eq!(sticky_runs, static_runs, "flow positions byte-identical to static");
        assert_eq!(sticky_h, static_h, "container height identical");
        // the guard has teeth: the same inset DOES move a relative box
        let (rel_runs, _) = build(Pos::Relative);
        assert_ne!(sticky_runs, rel_runs, "relative shifts by top:40 — sticky must not");
    }

    #[test]
    fn sticky_offset_pin_math() {
        let g = |doc_y: f32, doc_h: f32, top: f32, container_y1: f32| StickyGroup {
            shadows: (0, 0),
            rects: (0, 0),
            imgs: (0, 0),
            imgs_dec: (0, 0),
            runs: (0, 0),
            uls: (0, 0),
            doc_y,
            doc_h,
            top,
            container_y1,
            fixed: false,
        };
        // un-stuck: flow top still below the threshold → 0
        assert_eq!(sticky_offset(&g(100.0, 50.0, 40.0, 2000.0), 0.0), 0.0);
        assert_eq!(sticky_offset(&g(100.0, 50.0, 40.0, 2000.0), 50.0), 0.0);
        // scrolled past → pins: doc_y + off == scroll + top (240 = 200 + 40)
        assert_eq!(sticky_offset(&g(100.0, 50.0, 40.0, 2000.0), 200.0), 140.0);
        // clamped at the container bottom: box bottom lands on container_y1
        // (room = 2000 − (100 + 50))
        assert_eq!(sticky_offset(&g(100.0, 50.0, 40.0, 2000.0), 5000.0), 1850.0);
        // negative clamp range (box taller than its container) → no pin
        assert_eq!(sticky_offset(&g(100.0, 50.0, 40.0, 120.0), 5000.0), 0.0);
        // inside a fixed subtree nothing scrolls → threshold vs scroll 0
        let mut fg = g(10.0, 50.0, 40.0, 2000.0);
        fg.fixed = true;
        assert_eq!(sticky_offset(&fg, 9999.0), 30.0);
    }

    #[test]
    fn sticky_group_spans_cover_subtree_paint() {
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        // sticky parent with a background + a text child: both channels'
        // subtree entries must fall inside the group's recorded spans
        let mut head = pos_box(Pos::Sticky, [Some(Len::Px(0.0)), None, None, None], vec![leaf("title")]);
        head.bx.bg = Some(0x112233);
        // a threshold-less sticky never pins → it gets no group
        let bare = pos_box(Pos::Sticky, [None; 4], vec![leaf("bare")]);
        root.children = vec![head, bare];
        let l = layout_tree(&mut root, 300.0, &m4);
        assert_eq!(l.sticky.len(), 1, "only the top:0 box gets a group");
        let g = l.sticky[0];
        let ri = l.rects.iter().position(|r| r.4 == 0x112233).expect("bg rect");
        assert!(g.rects.0 <= ri && ri < g.rects.1, "rect {ri} outside {:?}", g.rects);
        let ti = l.runs.iter().position(|r| r.text == "title").expect("title run");
        assert!(g.runs.0 <= ti && ti < g.runs.1, "run {ti} outside {:?}", g.runs);
        assert_eq!(g.top, 0.0, "top:0 → pins at the viewport top");
    }

    #[test]
    fn page_without_sticky_is_untouched_and_paint_matches_static() {
        let build = |pos: Pos| {
            let mut root = BoxNode::group(Style::new(), BoxStyle::default());
            let mut box1 = pos_box(pos, [Some(Len::Px(40.0)), None, None, None], vec![leaf("one")]);
            box1.bx.bg = Some(0x224466);
            let box2 = pos_box(Pos::Static, [None; 4], vec![leaf("two")]);
            root.children = vec![box1, box2];
            layout_tree(&mut root, 300.0, &m4)
        };
        // a page without a sticky declaration records no groups …
        let plain = build(Pos::Static);
        assert!(plain.sticky.is_empty());
        // … and a sticky page's paint lists are byte-identical to the
        // static style's: layout/paint never offsets (pinning is render-
        // time only — regression guard for the never-scrolled page)
        let sticky = build(Pos::Sticky);
        assert_eq!(sticky.sticky.len(), 1);
        assert_eq!(sticky.rects, plain.rects, "bg rects identical");
        assert_eq!(sticky.underline_rects, plain.underline_rects, "underlines identical");
        assert_eq!(sticky.shadows, plain.shadows, "shadows identical");
        assert_eq!(sticky.images.len(), plain.images.len(), "images identical");
        assert_eq!(sticky.height, plain.height, "page height identical");
        let proj = |l: &Layout| {
            l.runs
                .iter()
                .map(|r| (r.x, r.y, r.width, r.text.clone(), r.fixed))
                .collect::<Vec<_>>()
        };
        assert_eq!(proj(&sticky), proj(&plain), "text runs identical");
    }

    #[test]
    fn sticky_inside_fixed_subtree_pins_without_scroll() {
        // a sticky box under position:fixed lives in viewport coordinates:
        // its group carries `fixed`, so pinning runs against scroll 0
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        let mut bar = pos_box(Pos::Fixed, [Some(Len::Px(0.0)), None, None, None], vec![]);
        let inner = pos_box(Pos::Sticky, [Some(Len::Px(10.0)), None, None, None], vec![leaf("stuck")]);
        // filler below gives the sticky box room to pin inside the bar
        let filler = pos_box(Pos::Static, [None; 4], vec![leaf("filler")]);
        bar.children = vec![inner, filler];
        root.children = vec![bar];
        let l = layout_in(&mut root, 300.0, 100.0, &m4);
        assert_eq!(l.sticky.len(), 1);
        let g = &l.sticky[0];
        assert!(g.fixed, "group inherits the fixed ancestor");
        assert_eq!(g.doc_y, 0.0, "laid at the viewport top");
        // scroll never applies inside the fixed subtree (999 ignored)
        assert_eq!(sticky_offset(g, 999.0), 10.0, "pins at top:10 of the viewport");
    }

    // ---- overflow: hidden / clip: rect() paint culling ----

    /// Box tree for the clip tests: `clipper` (100px wide, 2px border, bg,
    /// overflow: hidden) with three children — `inside` (fully in), a wide
    /// `straddle` (overflows right, must be cut) and `outside` (shifted far
    /// right by `position: relative`, must be dropped entirely).
    fn clip_tree(clip: bool) -> BoxNode {
        let mk = |color: u32, width: f32, kids: Vec<BoxNode>| {
            let mut g = BoxNode::group(
                Style::new(),
                BoxStyle {
                    display: Display::Block,
                    width: Some(Len::Px(width)),
                    bg: Some(color),
                    ..BoxStyle::default()
                },
            );
            g.children = kids;
            g
        };
        let mut clipper = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                width: Some(Len::Px(100.0)),
                border: [Len::Px(2.0); 4],
                bg: Some(0x112233),
                clip,
                ..BoxStyle::default()
            },
        );
        let inside = mk(0xff0000, 50.0, vec![leaf("ok")]);
        let straddle = mk(0x00ff00, 300.0, vec![leaf("over")]);
        let mut outside = mk(0x0000ff, 60.0, vec![leaf("gone")]);
        outside.bx.pos = Pos::Relative;
        outside.bx.inset = [None, None, None, Some(Len::Px(400.0))]; // left: shift +400
        clipper.children = vec![inside, straddle, outside];
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        root.children = vec![clipper];
        root
    }

    #[test]
    fn overflow_hidden_culls_and_intersects_descendants() {
        let mut root = clip_tree(true);
        let l = layout_tree(&mut root, 300.0, &m4);
        // own chrome is NOT clipped by own overflow: the bg rect stays the
        // full 100px BORDER box even though the padding box is only 96px
        // wide (2px borders) and content overflows past it
        let own = l
            .rects
            .iter()
            .find(|r| r.4 == 0x112233)
            .expect("own bg painted");
        assert!((own.2 - 100.0).abs() < 0.01, "own bg w={} = full border box", own.2);
        // fully-inside child: geometry untouched (content x=2, 50 wide)
        let ins = l.rects.iter().find(|r| r.4 == 0xff0000).expect("inside rect");
        assert!((ins.0 - 2.0).abs() < 0.01 && (ins.2 - 50.0).abs() < 0.01, "inside rect untouched: {ins:?}");
        // straddling child: intersected down to the padding box (x 2..98)
        let st = l.rects.iter().find(|r| r.4 == 0x00ff00).expect("straddle rect");
        assert!((st.0 - 2.0).abs() < 0.01, "straddle x={}", st.0);
        assert!((st.2 - 96.0).abs() < 0.01, "straddle w={} = padding box", st.2);
        // fully-outside child: rect dropped, not drawn at 400+
        assert!(
            !l.rects.iter().any(|r| r.4 == 0x0000ff),
            "outside rect culled: {:?}",
            l.rects
        );
        // runs carry the clip window; the inside run sits within it
        let ok = l.runs.iter().find(|r| r.text == "ok").expect("ok run");
        let (_, _, x1, y1) = ok.clip.expect("inside run clipped");
        assert!(ok.x >= 2.0 && ok.x + ok.width <= x1 + 0.01, "ok run inside window");
        assert!(ok.y + ok.style.size <= y1 + 0.01, "ok run inside window (y)");
        // the outside run's window excludes it entirely → main draws nothing
        let gone = l.runs.iter().find(|r| r.text == "gone").expect("gone run");
        let (_, _, gx1, _) = gone.clip.expect("outside run clipped");
        assert!(gone.x >= gx1, "gone run.x={} outside window x1={gx1}", gone.x);
    }

    #[test]
    fn overflow_visible_clips_nothing() {
        let mut root = clip_tree(false);
        let l = layout_tree(&mut root, 300.0, &m4);
        let st = l.rects.iter().find(|r| r.4 == 0x00ff00).expect("straddle rect");
        assert!((st.2 - 300.0).abs() < 0.01, "visible: straddle keeps full w={}", st.2);
        assert!(
            l.rects.iter().any(|r| r.4 == 0x0000ff),
            "visible: outside rect still painted"
        );
        assert!(
            l.runs.iter().all(|r| r.clip.is_none()),
            "visible: no run carries a clip window"
        );
    }

    #[test]
    fn nested_overflow_clips_compose_by_intersection() {
        // outer clips to 100px, inner to 150px — a 200px grandchild must end
        // up 100px wide (the INTERSECTION, not the inner box alone)
        let mut inner = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                width: Some(Len::Px(150.0)),
                bg: Some(0x223344),
                clip: true,
                ..BoxStyle::default()
            },
        );
        let mut grand = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                width: Some(Len::Px(200.0)),
                bg: Some(0xabcdef),
                ..BoxStyle::default()
            },
        );
        grand.children = vec![leaf("deep")];
        inner.children = vec![grand];
        let mut outer = BoxNode::group(
            Style::new(),
            BoxStyle {
                display: Display::Block,
                width: Some(Len::Px(100.0)),
                clip: true,
                ..BoxStyle::default()
            },
        );
        outer.children = vec![inner];
        let mut root = BoxNode::group(Style::new(), BoxStyle::default());
        root.children = vec![outer];
        let l = layout_tree(&mut root, 300.0, &m4);
        // grandchild bg: inner would allow 150, outer cuts to 100
        let g = l.rects.iter().find(|r| r.4 == 0xabcdef).expect("grand rect");
        assert!((g.2 - 100.0).abs() < 0.01, "grand w={} = outer ∩ inner", g.2);
        // inner's own bg is clipped by the OUTER overflow (ancestor chrome)
        let i = l.rects.iter().find(|r| r.4 == 0x223344).expect("inner rect");
        assert!((i.2 - 100.0).abs() < 0.01, "inner bg w={} cut by outer", i.2);
        // the run's window is the intersection too
        let run = l.runs.iter().find(|r| r.text == "deep").expect("deep run");
        let (_, _, x1, _) = run.clip.expect("clipped");
        assert!((x1 - 100.0).abs() < 0.01, "run window x1={x1} = composed");
    }

    #[test]
    fn skip_link_1px_clip_rect_paints_nothing() {
        // the classic visually-hidden pattern: absolutely positioned, no
        // insets, 1x1 with overflow:hidden + clip: rect(1px,1px,1px,1px).
        // The zero-area window must reach every run of the link so main
        // draws nothing, while page content is untouched.
        let dom = crate::html::parse(
            r##"<a class="skip" href="#main">Skip to content</a><p>page</p>"##,
        );
        let sheet = crate::css::parse(
            ".skip { position: absolute; width: 1px; height: 1px; \
             overflow: hidden; clip: rect(1px, 1px, 1px, 1px) }",
        );
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let l = layout_tree(&mut root, 300.0, &m4);
        // the link's words wrap in the 1px box — every one of its runs gets
        // a degenerate window (x0 >= x1 or y0 >= y1) that main culls
        let mut link_runs = 0;
        for r in l.runs.iter().filter(|r| r.text == "Skip" || r.text == "to" || r.text == "content") {
            let (x0, y0, x1, y1) = r.clip.expect("skip-link run clipped");
            assert!(
                x1 <= x0 || y1 <= y0,
                "run {:?} window ({x0},{y0},{x1},{y1}) must be empty",
                r.text
            );
            link_runs += 1;
        }
        assert!(link_runs >= 3, "found all wrapped skip-link runs: {link_runs}");
        // page content flows unclipped
        let page = l.runs.iter().find(|r| r.text == "page").expect("page run");
        assert!(page.clip.is_none(), "content run unclipped");
    }

    // ---- form controls (UA chrome: <select>/<input>/<textarea>) ----

    /// box tree from markup: parse + build over an empty sheet (UA chrome
    /// only — no author rules in the way).
    fn tree_of(markup: &str) -> BoxNode {
        let dom = crate::html::parse(markup);
        build(
            &dom,
            dom.body_root(),
            Style::new(),
            BoxStyle::default(),
            &crate::css::parse(""),
        )
    }

    /// every inline form-control box in tree order
    fn controls(n: &BoxNode) -> Vec<&BoxNode> {
        fn walk<'a>(n: &'a BoxNode, out: &mut Vec<&'a BoxNode>) {
            if n.inline {
                out.push(n);
            }
            for c in &n.children {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        walk(n, &mut out);
        out
    }

    #[test]
    fn select_renders_one_line_with_selected_option() {
        let mut root = tree_of(
            r#"<p>Lang <select><option>English</option><option selected>Français</option><option>Deutsch</option></select></p>"#,
        );
        {
            let cs = controls(&root);
            assert_eq!(cs.len(), 1, "select renders as exactly one box");
            let sel = cs[0];
            // one text leaf — the selected option — not three stacked labels
            assert_eq!(sel.children.len(), 1, "single text leaf, not one per option");
            assert!(sel.children[0].is_text);
            let t = &sel.children[0].text;
            assert!(t.contains("Français"), "selected label shown: {t:?}");
            assert!(
                !t.contains("English") && !t.contains("Deutsch"),
                "only the selected option: {t:?}"
            );
            assert!(t.contains('\u{25bc}'), "dropdown arrow on the line: {t:?}");
            // the box carries the UA chrome
            assert!(sel.bx.border[0] == Len::Px(1.0), "UA border");
            assert!(sel.bx.bg.is_some(), "UA background");
            // one line: the box does not stack the paragraph into 3+ lines
            let line = 15.0 * 1.35;
            assert!(
                root.h < line * 2.0,
                "h={} stays a single line",
                root.h
            );
        }
        let l = layout_tree(&mut root, 500.0, &m4);
        assert!(!l.runs.is_empty());
        let ys: Vec<f32> = l.runs.iter().map(|r| r.y).collect();
        let spread = ys.iter().cloned().fold(f32::MIN, f32::max)
            - ys.iter().cloned().fold(f32::MAX, f32::min);
        // inline on the same line (≤2px = the control's border/centering slack)
        assert!(spread <= 2.0, "runs share one line, y spread={spread}");
    }

    #[test]
    fn select_box_is_inline_and_shrinks_to_content() {
        let mut root =
            tree_of(r#"<div>Language <select><option>English</option></select> ok</div>"#);
        let sel;
        {
            let cs = controls(&root);
            assert_eq!(cs.len(), 1, "one control box");
            sel = cs[0];
            // structure: a real box (not flattened into the parent flow),
            // UA default display = joins the line, chrome filled in
            assert!(!sel.is_text, "boxed, not bare text");
            assert!(sel.bx.display == Display::Auto, "no author display → Auto");
            assert!(sel.inline, "inline-block placement");
            assert!(sel.bx.border[0] == Len::Px(1.0), "UA border");
            assert!(sel.bx.bg.is_some(), "UA background");
            assert!(sel.children.len() == 1 && sel.children[0].is_text);
            // first option (none marked selected) becomes the label
            assert!(sel.children[0].text.contains("English"), "first option");
        }
        let l = layout_tree(&mut root, 500.0, &m4);
        // sits ON the surrounding line: after "Language " (9 chars × 4px),
        // not on a stacked block line of its own
        let cs = controls(&root);
        let sel = cs[0];
        assert!((sel.x - 36.0).abs() < 0.01, "x={} = after \"Language \"", sel.x);
        assert!(sel.w > 0.0 && sel.w < 500.0, "shrink-to-fit w={}", sel.w);
        let lang = l.runs.iter().find(|r| r.text == "Language").expect("run");
        let eng = l.runs.iter().find(|r| r.text == "English").expect("label run");
        let ok = l.runs.iter().find(|r| r.text == "ok").expect("trailing run");
        assert!((eng.y - lang.y).abs() <= 2.0, "select on the text line");
        assert!((ok.y - lang.y).abs() <= 2.0, "text continues on that line");
        assert!(eng.x > lang.x + lang.width, "label starts inside the box");
    }

    #[test]
    fn inline_block_sits_on_the_text_line() {
        let dom = crate::html::parse(r#"<p>pre <span class="ib">mid</span> post</p>"#);
        let sheet = crate::css::parse(
            r#".ib { display: inline-block; background: #ff0000; border: 1px solid; padding: 2px; }"#,
        );
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        {
            let cs = controls(&root);
            assert_eq!(cs.len(), 1, "one inline-level box");
            let ib = cs[0];
            assert!(ib.bx.display == Display::InlineBlock, "display is InlineBlock");
            assert!(ib.inline, "inline placement");
            assert_eq!(ib.bx.bg, Some(0x00_ff_00_00), "background renders (real box)");
            assert!(ib.bx.border[0] == Len::Px(1.0), "border renders (real box)");
        }
        let l = layout_tree(&mut root, 500.0, &m4);
        let pre = l.runs.iter().find(|r| r.text == "pre").expect("pre run");
        let mid = l.runs.iter().find(|r| r.text == "mid").expect("mid run");
        let post = l.runs.iter().find(|r| r.text == "post").expect("post run");
        // all three on ONE line — the box's border+padding offset its inner
        // text a few px, but far less than a stacked line height (~20px);
        // the x-advancement below is the stronger same-line proof
        assert!((mid.y - pre.y).abs() < 10.0, "box on the text line (dy={})", mid.y - pre.y);
        assert!((post.y - pre.y).abs() < 10.0, "text continues after the box");
        // x advances left-to-right: pre -> mid (inside box) -> post
        assert!(pre.x < mid.x, "mid after pre: {} vs {}", mid.x, pre.x);
        assert!(mid.x < post.x, "post after mid: {} vs {}", post.x, mid.x);
    }

    #[test]
    fn inline_background_band_spans_text_and_padding() {
        // a plain inline with background + horizontal padding paints a band
        // across its full extent (text width + both padding sides), not just
        // behind each glyph - the span is promoted to a real inline box
        let dom = crate::html::parse(r#"<p>pre <span class="b">mid</span> post</p>"#);
        let sheet = crate::css::parse(r#".b { background: #ff0000; padding: 0 8px; }"#);
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        {
            let cs = controls(&root);
            assert_eq!(cs.len(), 1, "span promoted to one inline box");
            assert!(cs[0].bx.bg.is_some(), "background kept on the box");
            assert!(cs[0].inline, "inline placement");
        }
        let l = layout_tree(&mut root, 500.0, &m4);
        let band = controls(&root)[0];
        let pre = l.runs.iter().find(|r| r.text == "pre").expect("pre run");
        let mid = l.runs.iter().find(|r| r.text == "mid").expect("mid run");
        let post = l.runs.iter().find(|r| r.text == "post").expect("post run");
        // band spans text + 8px padding on each side: border-box width >= the
        // inner text width + 16 (the horizontal padding)
        let text_w = m4("mid", &Style::new());
        assert!(
            band.w >= text_w + 16.0 - 0.01,
            "band w={} spans text {} + 16px padding",
            band.w,
            text_w
        );
        // still one line: the band grows horizontally, not into a stacked row
        assert!((mid.y - pre.y).abs() < 10.0, "band on the text line");
        assert!((post.y - pre.y).abs() < 10.0, "text continues after the band");
    }

    #[test]
    fn inline_horizontal_margins_give_real_gaps() {
        // `.m{margin: 0 10px}` on plain inlines: each side contributes its
        // real width to the line (the old approximation emitted one literal
        // space ≈4px per side, which jammed/overlapped dense inline menus);
        // inline margins never collapse — adjacent boxes' margins add up.
        // This is the inline-block promotion path's margin sibling: the box
        // still sits ON the text line, margins don't stack it.
        let dom = crate::html::parse(r#"<div><span class="m">Main</span><span class="m">Talk</span></div>"#);
        let sheet = crate::css::parse(r#".m { margin: 0 10px; }"#);
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        {
            let cs = controls(&root);
            assert_eq!(cs.len(), 2, "both spans promoted to inline boxes");
            assert!(cs[0].inline && cs[1].inline, "inline placement");
            assert_eq!(cs[0].bx.margin[1], Len::Px(10.0), "right margin kept on the box");
            assert_eq!(cs[1].bx.margin[3], Len::Px(10.0), "left margin kept on the box");
        }
        let l = layout_tree(&mut root, 500.0, &m4);
        let main = l.runs.iter().find(|r| r.text == "Main").expect("Main run");
        let talk = l.runs.iter().find(|r| r.text == "Talk").expect("Talk run");
        // text advance = box border-box + 10px right margin + 10px left margin
        let gap = talk.x - (main.x + main.width);
        assert!(
            gap >= 18.0,
            "real margin gap: {gap} (want ≥ 20px margins minus slack; the old space ≈4px)"
        );
        assert!(talk.x >= main.x + main.width + 18.0, "no run overlap");
        // and <em> without margins still flattens exactly as before
        let dom2 = crate::html::parse(r#"<div><span>Main</span><span>Talk</span></div>"#);
        let mut root2 = build(&dom2, dom2.body_root(), Style::new(), BoxStyle::default(), &sheet);
        assert_eq!(controls(&root2).len(), 0, "margin-less inlines stay flattened");
        let l2 = layout_tree(&mut root2, 500.0, &m4);
        let t = l2.runs.iter().find(|r| r.text == "Talk").expect("Talk run");
        let main2 = l2.runs.iter().find(|r| r.text == "Main").expect("Main run");
        assert_eq!(t.x, main2.x + main2.width, "adjacent text, unchanged advance");
    }

    #[test]
    fn inline_margin_menu_advances_without_jam() {
        // header-menu shape: inline list items whose <a> labels previously
        // ran together ("Main PageTalk") — adjacent items get real gaps and
        // stay on one line
        let dom = crate::html::parse(
            r##"<ul><li><a href="#a">Main Page</a></li><li><a href="#b">Talk</a></li></ul>"##,
        );
        let sheet = crate::css::parse(r#"li { display: inline; margin: 0 6px; }"#);
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        {
            let cs: Vec<&BoxNode> = root
                .children[0]
                .children
                .iter()
                .collect();
            assert_eq!(cs.len(), 2, "both <li> promoted to inline boxes");
            assert!(cs[0].inline && cs[1].inline, "inline placement");
        }
        let l = layout_tree(&mut root, 500.0, &m4);
        let page = l.runs.iter().find(|r| r.text == "Page").expect("Page run");
        let talk = l.runs.iter().find(|r| r.text == "Talk").expect("Talk run");
        // second item starts after the first item's full extent + margins
        // (6px right + 6px left) — no jam, no overlap
        assert!(talk.x >= page.x + page.width + 10.0, "talk.x={} page end={}", talk.x, page.x + page.width);
        // one line: no stacking
        let main = l.runs.iter().find(|r| r.text == "Main").expect("Main run");
        assert!((main.y - talk.y).abs() < 10.0, "same line");
    }

    #[test]
    fn pct_width_inline_block_fills_containing_block() {
        // width:100% on an inline-block resolves against the containing
        // block's content width (the column), not the item's own natural
        // width - the button fills its 200px column
        let dom = crate::html::parse(r#"<div><span class="b">Go</span></div>"#);
        let sheet = crate::css::parse(r#".b { display: inline-block; width: 100%; }"#);
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        {
            let cs = controls(&root);
            assert_eq!(cs.len(), 1, "one inline box");
        }
        let l = layout_tree(&mut root, 200.0, &m4);
        let ib = controls(&root)[0];
        let go = l.runs.iter().find(|r| r.text == "Go").expect("Go run");
        assert!(go.x.abs() < 0.01, "starts at column left edge");
        assert!((ib.w - 200.0).abs() < 0.01, "box fills 200px column, w={}", ib.w);
    }

    #[test]
    fn two_inline_blocks_share_one_line() {
        let dom = crate::html::parse(r#"<div><span class="ib">A</span><span class="ib">B</span></div>"#);
        let sheet = crate::css::parse(r#".ib { display: inline-block; border: 1px solid; padding: 2px; }"#);
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        {
            let cs = controls(&root);
            assert_eq!(cs.len(), 2, "two inline-level boxes");
            assert!(cs[0].inline && cs[1].inline, "both inline placement");
        }
        let l = layout_tree(&mut root, 500.0, &m4);
        let a = l.runs.iter().find(|r| r.text == "A").expect("A run");
        let b = l.runs.iter().find(|r| r.text == "B").expect("B run");
        // side by side on one line: same y, B to the right of A
        assert!((b.y - a.y).abs() <= 2.0, "same line: A.y={} B.y={}", a.y, b.y);
        assert!(b.x > a.x, "B right of A: {} vs {}", b.x, a.x);
    }

    #[test]
    fn inline_block_wraps_when_it_does_not_fit() {
        // "xx" opens the line; the wide inline-block doesn't fit after it in
        // a 30px container, so it wraps to a fresh line below (not overflow)
        let dom = crate::html::parse(r#"<div>xx<span class="ib">WWWWWWWWWW</span></div>"#);
        let sheet = crate::css::parse(r#".ib { display: inline-block; }"#);
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let l = layout_tree(&mut root, 30.0, &m4);
        let xx = l.runs.iter().find(|r| r.text == "xx").expect("xx run");
        let w = l.runs.iter().find(|r| r.text.starts_with('W')).expect("box run");
        assert!(w.y > xx.y + 1.0, "wrapped below: xx.y={} box.y={}", xx.y, w.y);
    }

    #[test]
    fn inline_flex_and_inline_grid_still_stack_as_blocks() {
        // shortcut: inline-flex/inline-grid map to Block (no inline layout
        // mode) — they stack rather than sit mid-line; asserted so the
        // known gap is explicit
        let dom = crate::html::parse(r#"<div><span class="ib">A</span><span class="ib">B</span></div>"#);
        let sheet = crate::css::parse(r#".ib { display: inline-flex; }"#);
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let l = layout_tree(&mut root, 500.0, &m4);
        let a = l.runs.iter().find(|r| r.text == "A").expect("A run");
        let b = l.runs.iter().find(|r| r.text == "B").expect("B run");
        assert!(b.y > a.y + 1.0, "inline-flex stacks (documented shortcut)");
    }

    #[test]
    fn empty_select_renders_empty_box_without_crash() {
        let mut root = tree_of("<div><select></select></div>");
        {
            let cs = controls(&root);
            assert_eq!(cs.len(), 1, "empty select still gets its box");
            assert!(cs[0].children.is_empty(), "no options → no text, no arrow");
        }
        let l = layout_tree(&mut root, 200.0, &m4);
        assert!(l.runs.is_empty(), "nothing to draw");
        // and it flows inline between text without panicking
        let mut root2 = tree_of("<div>x<select></select>y</div>");
        let l2 = layout_tree(&mut root2, 200.0, &m4);
        let texts: Vec<&str> = l2.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["x", "y"]);
    }

    #[test]
    fn option_outside_select_still_renders_as_text() {
        let root = tree_of("<div><option>Bare</option></div>");
        assert!(controls(&root).is_empty(), "no box for a stray <option>");
        let div = &root.children[0];
        assert!(
            div.children.iter().any(|c| c.is_text && c.text == "Bare"),
            "its text still flows into the parent"
        );
    }

    #[test]
    fn author_css_overrides_ua_control_chrome() {
        let dom = crate::html::parse(r#"<div><select id="s"><option>X</option></select></div>"#);
        let sheet = crate::css::parse(
            r#"select { background: #112233; border: 3px solid #445566; padding: 5px; }"#,
        );
        let root = build(
            &dom,
            dom.body_root(),
            Style::new(),
            BoxStyle::default(),
            &sheet,
        );
        let cs = controls(&root);
        let sel = cs[0];
        assert_eq!(sel.bx.bg, Some(0x00_11_22_33), "author background wins");
        assert!(sel.bx.border == [Len::Px(3.0); 4], "author border wins");
        assert_eq!(sel.bx.bcolor, Some(0x00_44_55_66), "author border color");
        assert!(sel.bx.pad == [Len::Px(5.0); 4], "author padding wins");
        // undeclared properties keep the UA defaults
        assert!(sel.bx.radius == Len::Px(2.0), "UA radius survives");
        assert!(sel.inline, "still inline");
    }

    #[test]
    fn author_border_none_suppresses_ua_chrome() {
        // a RESET to initial must suppress the UA chrome (previously
        // indistinguishable from "not declared")
        let dom = crate::html::parse(r#"<div><select id="s"><option>X</option></select></div>"#);
        let sheet = crate::css::parse(
            r#"select { border: none; padding: 0; background: none; }"#,
        );
        let root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let sel = controls(&root)[0];
        assert!(sel.bx.border == [Len::Px(0.0); 4], "border:none suppresses UA border");
        assert!(sel.bx.pad == [Len::Px(0.0); 4], "padding:0 suppresses UA padding");
        assert_eq!(sel.bx.bg, None, "background:none suppresses UA fill");
    }

    #[test]
    fn author_colorless_border_uses_currentcolor_not_ua_gray() {
        // `border:1px solid` (no color) = currentcolor per spec, not UA gray
        let dom = crate::html::parse(r#"<div><input value="x"></div>"#);
        let sheet = crate::css::parse(r#"input { border: 1px solid; }"#);
        let root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let inp = controls(&root)[0];
        assert!(inp.bx.border == [Len::Px(1.0); 4], "declared width kept");
        assert_eq!(inp.bx.bcolor, None, "colorless border = currentcolor, not UA gray");
    }

    #[test]
    fn author_explicit_border_color_wins_over_ua_gray() {
        let dom = crate::html::parse(r#"<div><input value="x"></div>"#);
        let sheet = crate::css::parse(r#"input { border-color: #ff0000; }"#);
        let root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let inp = controls(&root)[0];
        assert_eq!(inp.bx.bcolor, Some(0x00_ff_00_00), "author border-color wins");
        assert!(inp.bx.border == [Len::Px(1.0); 4], "UA width survives a color-only decl");
    }

    #[test]
    fn input_and_textarea_render_value_boxes_hidden_skipped() {
        let root = tree_of(
            r#"<div><input type="hidden" value="secret"><input value="Hi"><textarea>Hello World</textarea></div>"#,
        );
        let cs = controls(&root);
        assert_eq!(cs.len(), 2, "hidden input skipped, visible ones boxed");
        assert_eq!(cs[0].children[0].text, "Hi", "input shows its value");
        assert!(
            cs[1].children[0].text.contains("Hello World"),
            "textarea shows its default value: {:?}",
            cs[1].children[0].text
        );
        // all three tags share the UA chrome
        for c in &cs {
            assert!(c.bx.border[0] == Len::Px(1.0) && c.bx.bg.is_some());
        }
        // no hidden value leaks into rendering
        assert!(controls(&root).iter().all(|c| !c.children.iter().any(|t| t.text.contains("secret"))));
    }

    // ---- form controls: focus / hit-test / values ----

    /// DOM node id of the i-th control box in tree order (every control
    /// box carries its FormCtl; tests need the stable node id).
    fn ctl_node(n: &BoxNode, i: usize) -> u32 {
        controls(n)[i].form.as_ref().expect("control box").node
    }

    #[test]
    fn controls_render_boxes_with_ua_default_sizes() {
        let mut root = tree_of(
            r#"<div><input value="Hi"><input type="checkbox"><input type="submit" value="Go"><button>OK</button></div>"#,
        );
        let cs = controls(&root);
        assert_eq!(cs.len(), 4, "input/checkbox/submit/button each get a box");
        // every control box is inline-level (joins the text flow)
        assert!(cs.iter().all(|c| c.inline), "controls flow inline");
        drop(cs);
        // text input: the classic ~170px default width even when filled
        let l = layout_in(&mut root, 500.0, 400.0, &m4);
        let text_input = l.forms.iter().find(|f| f.kind.takes_text()).expect("text box");
        assert!(
            (text_input.rect.2 - 170.0).abs() < 0.01,
            "filled text input keeps the 170px default width: {}",
            text_input.rect.2
        );
        assert!(text_input.rect.3 >= 20.0 - 0.01, "one line min-height");
        // checkbox: 14x14 square
        let cb = l.forms.iter().find(|f| f.kind == FormKind::Checkbox).expect("checkbox");
        assert!((cb.rect.2 - 14.0).abs() < 0.01 && (cb.rect.3 - 14.0).abs() < 0.01, "cb rect = {:?}", cb.rect);
        // submit + <button> element: real boxes too
        assert_eq!(l.forms.iter().filter(|f| f.kind.activates()).count(), 2, "submit + button");
    }

    #[test]
    fn value_attr_and_textarea_content_show_in_boxes() {
        let mut root = tree_of(
            r#"<div><input value="typed"><textarea>multi
line</textarea></div>"#,
        );
        let cs = controls(&root);
        assert_eq!(cs[0].children[0].text, "typed", "input shows value attr");
        assert!(
            cs[1].children[0].text.contains("multi"),
            "textarea content = its value"
        );
        // the laid output seeds the same raw values
        let l = layout_in(&mut root, 500.0, 400.0, &m4);
        let nodes: Vec<u32> = l.forms.iter().map(|f| f.node).collect();
        assert_eq!(nodes.len(), 2);
        assert_eq!(l.form_value(nodes[0]), "typed");
        assert!(l.form_value(nodes[1]).contains("multi"));
    }

    #[test]
    fn hit_test_form_inside_outside_and_scroll() {
        let mut root = tree_of(
            r#"<div style="margin-top:100px"><input value="box"></div>"#,
        );
        let l = layout_in(&mut root, 500.0, 400.0, &m4);
        assert_eq!(l.forms.len(), 1);
        let (x, y, w, h) = l.forms[0].rect;
        // inside the box (document coords == viewport coords at scroll 0)
        assert_eq!(l.hit_test_form(x + w / 2.0, y + h / 2.0, 0.0), Some(0));
        // outside: to the right of the box, and above it (margin area)
        assert_eq!(l.hit_test_form(x + w + 10.0, y + h / 2.0, 0.0), None);
        assert_eq!(l.hit_test_form(x + w / 2.0, y - 10.0, 0.0), None);
        // scroll respected: the same viewport point maps 100px down the doc
        assert_eq!(l.hit_test_form(x + w / 2.0, y + h / 2.0 - 100.0, 100.0), Some(0));
        assert_eq!(l.hit_test_form(x + w / 2.0, y + h / 2.0, 100.0), None);
    }

    // ---- hover hit-testing + cursor ----

    /// DOM node id of the element carrying `id=<want>` — hover targets
    /// carry node ids, so tests need the stable ones.
    fn id_by_attr(dom: &Dom, want: &str) -> u32 {
        (0..dom.nodes.len() as u32)
            .find(|&n| dom.attr(n, "id") == Some(want))
            .unwrap_or_else(|| panic!("no element with id={want}"))
    }

    #[test]
    fn hover_hit_test_returns_node_and_later_box_wins() {
        // shortcut: plain blocks size to content (declared `height` only
        // applies to replaced elements) — min-height is the honored floor
        let markup = "<div id=\"c\" style=\"width:50px;min-height:200px\"></div>\
            <div id=\"a\" style=\"position:absolute;left:10px;top:10px;width:50px;min-height:50px\"></div>\
            <div id=\"b\" style=\"position:absolute;left:10px;top:10px;width:50px;min-height:50px;cursor:pointer\"></div>";
        let dom = crate::html::parse(markup);
        let sheet = crate::css::parse("");
        let (c, b) = (id_by_attr(&dom, "c"), id_by_attr(&dom, "b"));
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let l = layout_in(&mut root, 300.0, 200.0, &m4);
        // a and b overlap at (15,15); b is LATER in tree order so it wins
        // the reverse scan — and its inline `cursor:pointer` rides along
        assert_eq!(
            l.hit_hover(15.0, 15.0, 0.0),
            Some((b, Some(Cursor::Pointer)))
        );
        // scroll math (same as the form/link hit-tests): viewport y=-85
        // with scroll=100 is document y=15 → still over b
        assert_eq!(
            l.hit_hover(15.0, -85.0, 100.0),
            Some((b, Some(Cursor::Pointer)))
        );
        // scrolled down: document y=115 falls in the tall static box c
        // (no cursor declared → None), past a/b's 50px band
        assert_eq!(l.hit_hover(15.0, 15.0, 100.0), Some((c, None)));
        // outside every element box: the root (body/document) box covers
        // this point but is filtered — it must never become the hover cell
        assert_eq!(l.hit_hover(250.0, 100.0, 0.0), None);
    }

    #[test]
    fn hover_text_run_targets_carry_node_and_cursor() {
        // min-height, not height (declared height only applies to replaced
        // elements) — the empty area below the text line is still a target
        let markup =
            r#"<div id="c" style="width:200px;min-height:60px">ab<span id="s" style="cursor:text">cd</span></div>"#;
        let dom = crate::html::parse(markup);
        let sheet = crate::css::parse("");
        let (c, s) = (id_by_attr(&dom, "c"), id_by_attr(&dom, "s"));
        let mut root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        let l = layout_in(&mut root, 300.0, 200.0, &m4);
        // the span's text is its OWN run target: node = the span, cursor
        // inherited down the box tree at build time
        let cd = l.runs.iter().find(|r| r.text == "cd").expect("cd run");
        assert_eq!(
            l.hit_hover(cd.x + 1.0, cd.y + 1.0, 0.0),
            Some((s, Some(Cursor::Text)))
        );
        // empty area of the div (below the text line): the BOX target,
        // which declared no cursor
        assert_eq!(l.hit_hover(150.0, 50.0, 0.0), Some((c, None)));
    }

    #[test]
    fn cursor_inherits_down_the_box_tree_at_build() {
        let markup = r#"<div id="d"><p id="p"><span id="s">x</span></p></div>"#;
        let dom = crate::html::parse(markup);
        let sheet = crate::css::parse("div { cursor: crosshair }");
        let (d, p, s) = (id_by_attr(&dom, "d"), id_by_attr(&dom, "p"), id_by_attr(&dom, "s"));
        let root = build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet);
        // the declaring box…
        let div_box = &root.children[0];
        assert_eq!(div_box.node, Some(d));
        assert_eq!(div_box.bx.cursor, Some(Cursor::Crosshair));
        // …the intermediate <p>, which declares nothing (cursor is not a
        // cascade-inherited property — build threads the parent's value)…
        let p_box = &div_box.children[0];
        assert_eq!(p_box.node, Some(p));
        assert_eq!(p_box.bx.cursor, Some(Cursor::Crosshair));
        // …and the flattened <span>'s text leaf: node = the span (its
        // containing element), cursor already stamped
        let leaf = &p_box.children[0];
        assert!(leaf.is_text && leaf.text == "x");
        assert_eq!(leaf.node, Some(s));
        assert_eq!(leaf.bx.cursor, Some(Cursor::Crosshair));
        // no declaration anywhere above → None, not a default pointer
        let bare_dom = crate::html::parse(r#"<div id="d2"><span id="s2">x</span></div>"#);
        let s2 = id_by_attr(&bare_dom, "s2");
        let bare = boxes_of(r#"<div id="d2"><span id="s2">x</span></div>"#, "");
        let div_box = &bare.children[0];
        assert_eq!(div_box.bx.cursor, None);
        let leaf = &div_box.children[0];
        assert_eq!(leaf.node, Some(s2));
        assert_eq!(leaf.bx.cursor, None);
    }

    #[test]
    fn focus_survives_relayout_by_node_id() {
        let markup = r#"<div><input value="one"><input value="two"></div>"#;
        // layout at width A, focus field 2
        let node2;
        let state;
        {
            let mut root = tree_of(markup);
            node2 = ctl_node(&root, 1);
            let st = FormState {
                focused: Some(node2),
                values: Default::default(),
            };
            let l = layout_in_forms(&mut root, 250.0, 400.0, &m4, &st);
            assert!(l.forms[1].focused, "field 2 focused at width A");
            assert!(!l.forms[0].focused, "field 1 not focused");
            // the focus ring changed field 2's border to the accent
            assert_eq!(
                root.children[0].children[1].bx.bcolor,
                Some(FORM_ACCENT),
                "focused field border = accent"
            );
            state = st;
        }
        // rebuild at a different width (fresh box tree — like a resize),
        // carrying the same state: same node id still focused
        let mut root = tree_of(markup);
        assert_eq!(ctl_node(&root, 1), node2, "node ids stable across rebuild");
        let l = layout_in_forms(&mut root, 600.0, 400.0, &m4, &state);
        let focused = l.forms.iter().find(|f| f.focused).expect("one focused");
        assert_eq!(focused.node, node2, "focus maps by node id, not index");
        assert!((focused.rect.2 - 170.0).abs() < 0.01, "re-laid at the new width");
    }

    #[test]
    fn password_masks_display_but_stores_raw() {
        let mut root = tree_of(r#"<div><input type="password" value="hunter2"></div>"#);
        {
            let cs = controls(&root);
            assert_eq!(cs[0].children[0].text, "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}", "build shows 7 bullets");
        }
        let l = layout_in(&mut root, 500.0, 400.0, &m4);
        let node = l.forms[0].node;
        assert_eq!(l.form_value(node), "hunter2", "storage keeps the raw text");
        // live typing: display masks, storage stays raw
        let mut l = l;
        l.set_form_value(node, "pw");
        let st = l.form_state();
        let mut root = tree_of(r#"<div><input type="password" value="hunter2"></div>"#);
        let l = layout_in_forms(&mut root, 500.0, 400.0, &m4, &st);
        assert_eq!(l.runs.iter().map(|r| r.text.as_str()).find(|t| t.contains('\u{2022}')).unwrap(), "\u{2022}\u{2022}", "2-char value → 2 bullets shown");
        assert_eq!(l.form_value(node), "pw", "raw after edit");
    }

    #[test]
    fn submit_activates_from_hit_test() {
        let mut root = tree_of(
            r#"<div><input value="q"><input type="submit" value="Go"></div>"#,
        );
        let l = layout_in(&mut root, 500.0, 400.0, &m4);
        let (sx, sy, sw, sh) = l.forms[1].rect;
        let i = l.hit_test_form(sx + sw / 2.0, sy + sh / 2.0, 0.0).expect("hit submit");
        assert!(l.forms[i].kind.activates(), "submit kind activates");
        assert!(!l.forms[0].kind.activates(), "text field does not");
        // <button> element behaves the same
        let mut root = tree_of(r#"<div><button>OK</button></div>"#);
        let l = layout_in(&mut root, 500.0, 400.0, &m4);
        let (bx, by, bw, bh) = l.forms[0].rect;
        let i = l.hit_test_form(bx + bw / 2.0, by + bh / 2.0, 0.0).expect("hit button");
        assert!(l.forms[i].kind.activates());
    }

    #[test]
    fn button_element_centers_its_label() {
        let mut root = tree_of(r#"<div><button>A pretty long label</button></div>"#);
        // the label leaf carries the UA centered text-align, and with an
        // author-set width the slack lands on both sides evenly
        assert_eq!(controls(&root)[0].children[0].style.text_align, 1, "UA centers button labels");
        let mut root = tree_of(r#"<div><button style="width:300px">Hi</button></div>"#);
        let l = layout_in(&mut root, 500.0, 400.0, &m4);
        let (x, _, w, _) = l.forms[0].rect;
        assert!((w - 300.0).abs() < 0.01, "author width wins: {w}");
        let run = l.runs.iter().find(|r| r.text == "Hi").expect("label run");
        let slack = (w - m4("Hi", &Style::new()) - 8.0).max(0.0) / 2.0; // minus border+pad
        assert!(
            (run.x - (x + 4.0 + slack)).abs() < 2.0,
            "label centered in the slack: run.x={} expected≈{}",
            run.x,
            x + 4.0 + slack
        );
    }

    #[test]
    fn long_value_tail_clips_at_box_edge() {
        let long = "a".repeat(200);
        let markup = format!(r#"<div><input value="{long}"></div>"#);
        let mut root = tree_of(&markup);
        let l = layout_in(&mut root, 800.0, 400.0, &m4);
        let node = l.forms[0].node;
        // storage keeps the full value
        assert_eq!(l.form_value(node).chars().count(), 200);
        // the shown text is clipped to the box's content width (~162px
        // content / 4px per char ≈ 40 chars)
        let shown = l.runs.iter().map(|r| r.text.as_str()).find(|t| !t.is_empty() && t.starts_with('a')).expect("value run");
        assert!(
            m4(shown, &Style::new()) <= 170.0 - 6.0 - 2.0 + 0.01,
            "clipped to content width: {}px ({} chars)",
            m4(shown, &Style::new()),
            shown.chars().count()
        );
        assert!(shown.chars().count() < 200, "tail was cut");
    }

    // ---- z-index (stacking) + transform (paint-only) ----

    #[test]
    fn z_index_orders_positioned_backgrounds() {
        // three stacked overlays whose document order runs AGAINST their
        // z order: paint must follow ascending z, not tree order
        let mut root = boxes_of(
            "<div class=hi>a</div><div class=mid>b</div><div class=lo>c</div>",
            ".hi{position:absolute;z-index:10;background:#333333}\
             .mid{position:relative;z-index:5;background:#222222}\
             .lo{position:absolute;z-index:2;background:#111111}",
        );
        let l = layout_tree(&mut root, 300.0, &m4);
        let idx = |c: u32| l.rects.iter().position(|r| r.4 == c).expect("rect");
        // relative boxes with an integer z stack too (mid stays in flow but
        // paints at level 5 — above lo's 2, below hi's 10)
        assert!(idx(0x111111) < idx(0x222222), "z:2 under z:5");
        assert!(idx(0x222222) < idx(0x333333), "z:5 under z:10");
    }

    #[test]
    fn z_index_auto_and_static_stay_in_tree_order() {
        // `z-index: auto` (and a z-index on a STATIC box, which spec
        // ignores) keeps the long-standing tree-order paint: earlier in
        // the document paints first
        let mut root = boxes_of(
            "<div class=first>a</div><div class=second>b</div>",
            ".first{position:absolute;background:#aaaaaa}\
             .second{position:absolute;z-index:auto;background:#bbbbbb}\
             .stat{z-index:99}",
        );
        let l = layout_tree(&mut root, 300.0, &m4);
        let idx = |c: u32| l.rects.iter().position(|r| r.4 == c).expect("rect");
        assert!(idx(0xaaaaaa) < idx(0xbbbbbb), "auto → tree order");
        // z on a static box never lifts it: append a static bg AFTER the
        // absolute one — it still paints at level 0, i.e. FIRST
        let mut root = boxes_of(
            "<div class=abs>a</div><div class=stat>b</div>",
            ".abs{position:absolute;z-index:2;background:#cccccc}\
             .stat{z-index:99;background:#dddddd}",
        );
        let l = layout_tree(&mut root, 300.0, &m4);
        let idx = |c: u32| l.rects.iter().position(|r| r.4 == c).expect("rect");
        assert!(idx(0xdddddd) < idx(0xcccccc), "static z ignored → level 0");
    }

    #[test]
    fn negative_z_index_paints_under_inflow_content() {
        // the classic decoration-layer pattern: a negative z paints BELOW
        // the in-flow content of the stacking root — even when the
        // decoration comes LATER in the document (documented model: it
        // sorts at the root level, under level-0 backgrounds)
        let mut root = boxes_of(
            "<div class=flow>f</div><div class=under>u</div>",
            ".under{position:absolute;z-index:-1;background:#333333}\
             .flow{background:#cccccc}",
        );
        let l = layout_tree(&mut root, 300.0, &m4);
        let idx = |c: u32| l.rects.iter().position(|r| r.4 == c).expect("rect");
        assert!(idx(0x333333) < idx(0xcccccc), "negative z under in-flow bg");
    }

    #[test]
    fn z_index_orders_text_runs_and_images() {
        // the text/image channels carry the same stamped levels as rects:
        // a z:10 header FIRST in the document paints LAST among the
        // absolutely positioned boxes (tree order would paint it first)
        let mut root = boxes_of(
            "<div class=hi>head</div><div class=lo>other</div>",
            ".hi{position:absolute;z-index:10;top:0;background:#eeeeee}\
             .lo{position:absolute;z-index:2;top:30px;background:#dddddd}",
        );
        // slot a content <img> into each subtree (Ready → image channel)
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 1, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 255]).unwrap();
        }
        let px = std::rc::Rc::new(crate::image::decode(&bytes).expect("png decodes"));
        for i in 0..2 {
            let mut g = BoxNode::group(Style::new(), BoxStyle::default());
            g.img = Some(ImgSlot::Ready(px.clone()));
            root.children[i].children.push(g);
        }
        let l = layout_tree(&mut root, 300.0, &m4);
        // header text paints after the lower-z box's text
        let run_idx = |t: &str| l.runs.iter().position(|r| r.text == t).expect("run");
        assert!(run_idx("head") > run_idx("other"), "z:10 text after z:2 text");
        // ...and its image too: the z:2 box sits at y=30, the z:10 one at
        // y=0, so a z-sorted list ENDS with the y=0 image (tree order
        // would start with it)
        assert_eq!(l.images.len(), 2);
        assert!(
            l.images[0].1 > l.images[1].1,
            "z:10 image paints last: first y={} > second y={}",
            l.images[0].1,
            l.images[1].1
        );
    }

    #[test]
    fn transform_translate_moves_paint_not_layout() {
        // the header hide/show pattern: translateY(-100%) moves the box's
        // WHOLE subtree's painting (bg, children, text) while layout
        // positions — the box's flow slot and its sibling — never move
        let build = |css: &str| {
            let mut root = boxes_of(
                "<div class=t><div class=kid>k</div></div><div class=s>sib</div>",
                css,
            );
            let l = layout_tree(&mut root, 300.0, &m4);
            (root, l)
        };
        let t_css = ".t{min-height:50px;background:#aabbcc;transform:translateY(-100%)}\
                     .kid{background:#ddeeff}";
        let (root, l) = build(t_css);
        // flow: the box and its sibling sit exactly where they would
        // without the transform
        assert!((root.children[0].y - 0.0).abs() < 0.01, "t stays at y=0 in flow");
        assert!((root.children[1].y - 50.0).abs() < 0.01, "sib still at y=50");
        // paint: both the box's bg and its CHILD's bg moved up 50px
        let rect = |c: u32| l.rects.iter().find(|r| r.4 == c).expect("rect");
        assert!((rect(0xaabbcc).1 - -50.0).abs() < 0.01, "bg painted at y=-50");
        assert!((rect(0xddeeff).1 - -50.0).abs() < 0.01, "child bg moved with the box");
        // text: the run moved too (translate is exact for glyphs)
        let kid_run = l.runs.iter().find(|r| r.text == "k").expect("kid run");
        let (plain_root, plain) = build(
            ".t{min-height:50px;background:#aabbcc}.kid{background:#ddeeff}",
        );
        let plain_run = plain.runs.iter().find(|r| r.text == "k").expect("kid run");
        assert!(
            (kid_run.y - (plain_run.y - 50.0)).abs() < 0.01,
            "run moved by exactly the translate ({} vs {})",
            kid_run.y,
            plain_run.y
        );
        let _ = plain_root;
    }

    #[test]
    fn transform_scale_doubles_painted_rect_about_center() {
        // scale(2) about the default origin (box center): the painted rect
        // doubles in size and recenters; layout w/h and the sibling stay
        // put (compare against a plain twin — flow is byte-identical).
        // glyph size does NOT scale (documented shortcut) — only the anchor
        let build = |css: &str| {
            let mut root = boxes_of("<div class=t>head</div><div class=s>sib</div>", css);
            let l = layout_tree(&mut root, 400.0, &m4);
            (root, l)
        };
        let (root, l) = build(".t{width:100px;min-height:50px;background:#aabbcc;transform:scale(2)}");
        let (plain, plain_l) = build(".t{width:100px;min-height:50px;background:#aabbcc}");
        assert!((root.children[0].w - 100.0).abs() < 0.01, "layout w unchanged");
        assert!(
            (root.children[1].y - plain.children[1].y).abs() < 0.01,
            "sibling unmoved"
        );
        let r = l.rects.iter().find(|r| r.4 == 0xaabbcc).expect("bg");
        assert!(
            (r.0 - -50.0).abs() < 0.01
                && (r.1 - -25.0).abs() < 0.01
                && (r.2 - 200.0).abs() < 0.01
                && (r.3 - 100.0).abs() < 0.01,
            "painted rect = scaled about center: {r:?}"
        );
        // text anchor recenters with the scale; glyphs keep their size
        let run = l.runs.iter().find(|r| r.text == "head").expect("run");
        let plain_run = plain_l.runs.iter().find(|r| r.text == "head").expect("run");
        assert!(
            (run.x - (plain_run.x * 2.0 - 50.0)).abs() < 0.01,
            "run anchor scaled: {} (plain {})",
            run.x,
            plain_run.x
        );
        assert!(
            (run.style.size - plain_run.style.size).abs() < 0.01,
            "glyph size unchanged"
        );
    }

    #[test]
    fn transform_none_and_absent_paint_identically() {
        // regression guard: an explicit `transform:none` and a page with
        // no transform at all produce byte-identical paint lists
        let build = |css: &str| {
            let mut root = boxes_of(
                "<div class=t>head</div><div class=s>sib</div>",
                css,
            );
            layout_tree(&mut root, 300.0, &m4)
        };
        let none = build(".t{min-height:50px;background:#aabbcc;transform:none}.s{min-height:30px}");
        let absent = build(".t{min-height:50px;background:#aabbcc}.s{min-height:30px}");
        assert_eq!(none.rects.len(), 1);
        assert_eq!(none.rects, absent.rects);
        let runs = |l: &Layout| {
            l.runs
                .iter()
                .map(|r| (r.text.clone(), r.x, r.y, r.width))
                .collect::<Vec<_>>()
        };
        assert_eq!(runs(&none), runs(&absent));
    }

    #[test]
    fn transform_rotate_and_skew_are_documented_noops() {
        // rotate()/skew() parse but never rasterize (affine glyph/rect
        // sampling is out of scope) — a rotate/skew-only value paints
        // exactly like none; a co-listed translate still applies
        let build = |css: &str| {
            let mut root = boxes_of("<div class=t>x</div>", css);
            layout_tree(&mut root, 300.0, &m4)
        };
        let plain = build(".t{min-height:40px;background:#aabbcc}");
        let rot = build(".t{min-height:40px;background:#aabbcc;transform:rotate(45deg) skew(3deg, 2deg)}");
        assert_eq!(rot.rects, plain.rects, "rotate/skew-only = no transform");
        let tx = build(".t{min-height:40px;background:#aabbcc;transform:rotate(45deg) translateX(10px)}");
        assert!((tx.rects[0].0 - 10.0).abs() < 0.01, "co-listed translate still applies");
    }

    #[test]
    fn transformed_overflow_clip_travels_with_the_box() {
        // the box's overflow clip window is mapped through its transform,
        // so its child's painting is clipped to the SHIFTED padding box —
        // the child rides the transform but stays cut at the box edge
        let mut root = boxes_of(
            "<div class=t><div class=c>k</div></div>",
            ".t{width:100px;min-height:100px;overflow:hidden;transform:translateX(30px);background:#aabbcc}\
             .c{width:200px;min-height:20px;background:#ddeeff}",
        );
        let l = layout_tree(&mut root, 400.0, &m4);
        let t = l.rects.iter().find(|r| r.4 == 0xaabbcc).expect("own bg");
        assert!((t.0 - 30.0).abs() < 0.01, "own bg shifted: {t:?}");
        let c = l.rects.iter().find(|r| r.4 == 0xddeeff).expect("child bg");
        assert!(
            (c.0 - 30.0).abs() < 0.01 && (c.2 - 100.0).abs() < 0.01,
            "child clipped to the shifted padding box: {c:?}"
        );
    }

    // ---- background-image (url() painting) ----

    /// html + stylesheet → laid root box tree (body box = root).
    fn boxes_of(markup: &str, css: &str) -> BoxNode {
        let dom = crate::html::parse(markup);
        let sheet = crate::css::parse(css);
        build(&dom, dom.body_root(), Style::new(), BoxStyle::default(), &sheet)
    }

    // ---- ::before/::after generated content ----

    #[test]
    fn pseudo_before_is_first_child_and_after_last() {
        let root = boxes_of(
            "<div>hi</div>",
            r#"div::before { content: "[1]" } div::after { content: "[2]" }"#,
        );
        let div = &root.children[0];
        assert_eq!(div.children.len(), 3, "before + content + after");
        assert!(
            div.children[0].is_text && div.children[0].text == "[1]",
            "::before is the first child"
        );
        assert!(div.children[1].is_text && div.children[1].text == "hi");
        assert!(
            div.children[2].is_text && div.children[2].text == "[2]",
            "::after is the last child"
        );
    }

    #[test]
    fn legacy_single_colon_pseudo_and_body_root_boxes() {
        // legacy `:before` spelling — a plain inline span flattens, so its
        // pseudo joins the parent flow right before the element's own text
        let root = boxes_of("<span>x</span>", r#"span:before { content: "L" }"#);
        assert!(root.children[0].is_text && root.children[0].text == "L");
        assert!(root.children[1].is_text && root.children[1].text == "x");
        // the body's own box gets them too (root = the real <body> element)
        let root = boxes_of(
            "<body>x</body>",
            r#"body::before { content: "B" } body::after { content: "A" }"#,
        );
        assert!(root.children[0].is_text && root.children[0].text == "B");
        assert!(root.children[1].is_text && root.children[1].text == "x");
        assert!(root.children[2].is_text && root.children[2].text == "A");
    }

    #[test]
    fn pseudo_generates_no_box_without_usable_content() {
        // a matching rule that never declares `content` → no box
        let a = boxes_of("<div>hi</div>", "div::before { color: #ff0000 }");
        assert_eq!(a.children[0].children.len(), 1, "undeclared content → no box");
        // `content: none` kills the box even when an earlier rule set one
        let b = boxes_of(
            "<div>hi</div>",
            r#"div::before { content: "x" } div::before { content: none }"#,
        );
        assert_eq!(b.children[0].children.len(), 1, "content:none → no box");
        // `display:none` on the pseudo kills it too
        let c = boxes_of(
            "<div>hi</div>",
            r#"div::before { content: "x"; display: none }"#,
        );
        assert_eq!(c.children[0].children.len(), 1, "display:none → no box");
    }

    #[test]
    fn pseudo_box_carries_its_own_style_not_the_elements() {
        let root = boxes_of(
            "<div>hi</div>",
            r#"div { color: #111111 }
               div::before { content: "x"; color: #ff0000 }
               div::after { content: "y"; color: #ff0000; background: #0000ff }"#,
        );
        let div = &root.children[0];
        // no chrome → a bare text run carrying the pseudo's own color
        let before = &div.children[0];
        assert!(before.is_text && before.text == "x");
        assert_eq!(before.style.color, 0xff0000, "the pseudo's color, not the element's");
        // chrome → an inline box wrapping the generated text
        let after = &div.children[2];
        assert!(!after.is_text, "background wraps the run in a box");
        assert!(after.inline, "inline-level box on the text line");
        assert_eq!(after.bx.bg, Some(0x0000ff), "the pseudo's own background");
        assert_eq!(after.style.color, 0xff0000);
        assert!(after.children[0].is_text && after.children[0].text == "y");
        // the element's own cascade is untouched by the pseudo rules
        assert_eq!(div.style.color, 0x111111);
        assert!(div.children[1].is_text && div.children[1].text == "hi");
    }

    #[test]
    fn pseudo_content_flows_with_surrounding_text() {
        let mut root = boxes_of("<p>hi</p>", r#"p::before { content: "[1]" }"#);
        let l = layout_tree(&mut root, 500.0, &m4);
        let joined: String = l.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(joined, "[1]hi", "generated run joins the paragraph's text");
        assert!(
            l.runs.windows(2).all(|w| w[0].y == w[1].y),
            "same line — the pseudo box doesn't break the flow"
        );
    }

    #[test]
    fn pseudo_rules_without_usable_content_leave_layout_untouched() {
        // regression guard: pseudo rules that generate no box must lay out
        // EXACTLY like a sheet without them (BoxNode has no Debug — compare
        // a structural + geometric dump of the tree and the text runs)
        fn shape(n: &BoxNode) -> String {
            let mut s = format!(
                "{}|{}|{:.2},{:.2},{:.2},{:.2};",
                n.is_text, n.text, n.x, n.y, n.w, n.h
            );
            for c in &n.children {
                s.push_str(&shape(c));
            }
            s.push('|');
            s
        }
        let laid = |css: &str| {
            let mut root = boxes_of("<p>one <b>two</b></p><span>three</span>", css);
            let l = layout_tree(&mut root, 1000.0, &m4);
            let runs: Vec<(String, f32, f32)> =
                l.runs.iter().map(|r| (r.text.clone(), r.x, r.y)).collect();
            (shape(&root), runs)
        };
        let plain = laid("p { color: #123456 }");
        // a matched pseudo rule without `content` changes nothing
        assert_eq!(
            laid("p { color: #123456 } p::before { color: #654321 }"),
            plain,
            "content-less pseudo rule changes nothing"
        );
        // and so does an explicit `content: none`
        assert_eq!(
            laid("p { color: #123456 } p::before { content: none }"),
            plain,
            "content:none generates nothing"
        );
    }

    #[test]
    fn background_image_url_parsed_and_resolved() {
        // inline style: url() lands on the box as a Pending slot inside a
        // synthetic placeholder child (child 0 → paints under content)
        let root = boxes_of(r#"<div style="background-image: url(img/tri.png)">x</div>"#, "");
        let div = &root.children[0];
        assert!(div.children[0].is_bg, "placeholder is the first child");
        match &div.children[0].img {
            Some(ImgSlot::Pending(src)) => assert_eq!(&**src, "img/tri.png"),
            _ => panic!("Pending slot expected on the placeholder"),
        }
        // the shared <img> pipeline resolves that raw src against the page
        // URL exactly like an <img src> (probe_images calls resolve first)
        assert_eq!(
            crate::resolve("http://host.test/p/page.html", "img/tri.png").as_deref(),
            Some("http://host.test/p/img/tri.png"),
            "raw url joins the page base to an absolute URL"
        );
    }

    #[test]
    fn background_shorthand_url_case_kept_and_reset() {
        let root = boxes_of(
            r#"<div class="a">x</div><div class="a b">y</div><div class="c">z</div>"#,
            r#"
                .a { background: url(/Assets/Icon.PNG) no-repeat }
                .b { background: #fff }
                .c { background: url(c.png); background-image: none }
            "#,
        );
        let bg_pending = |i: usize| match &root.children[i].children[0].img {
            Some(ImgSlot::Pending(s)) => Some(&**s),
            _ => None,
        };
        // shorthand url survives keywords after it, raw case preserved
        assert_eq!(bg_pending(0), Some("/Assets/Icon.PNG"));
        // spec: a later color-only `background:` shorthand resets the image
        assert_eq!(bg_pending(1), None, "color-only background: clears url");
        // longhand `background-image: none` also clears it
        assert_eq!(bg_pending(2), None, "background-image: none clears url");
    }

    #[test]
    fn background_image_paints_under_content_at_natural_size() {
        // a real 2×1 PNG written here; the pipeline would decode it in
        // decode_images (main.rs) — slotting Ready directly proves the
        // paint path: natural size, padding-box top-left, listed BEFORE the
        // content <img> (render draws images in list order, all under text)
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, 2, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 128, 0, 255, 0, 255]).unwrap();
        }
        let img = std::rc::Rc::new(crate::image::decode(&bytes).expect("png decodes"));
        let mut root = boxes_of(
            r#"<div style="background-image:url(t.png)"><img src="c.png">text</div>"#,
            "",
        );
        let div = &mut root.children[0];
        assert!(div.children[0].is_bg);
        div.children[0].img = Some(ImgSlot::Ready(img.clone()));
        let content = div
            .children
            .iter_mut()
            .find(|c| !c.is_bg && c.img.is_some())
            .expect("content <img> box");
        content.img = Some(ImgSlot::Ready(img));
        let l = layout_tree(&mut root, 500.0, &m4);
        assert_eq!(l.images.len(), 2, "bg + content img");
        let (x, y, w, h, _, _) = l.images[0];
        assert_eq!((x, y, w, h), (0.0, 0.0, 2.0, 1.0), "natural size at padding-box origin");
        // z-order: the bg entry precedes the content image; text runs draw
        // after ALL images in render(), so the text sits on top
        assert!(!l.runs.is_empty(), "text content unaffected");
    }

    #[test]
    fn background_image_fetch_failure_paints_nothing() {
        // probe/fetch failed → the slot stays Pending → no decode, no draw
        // entry, no panic, content unaffected
        let mut root = boxes_of(r#"<div style="background-image:url(missing.png)">x</div>"#, "");
        let l = layout_tree(&mut root, 500.0, &m4);
        assert!(l.images.is_empty());
        assert!(!l.runs.is_empty());
    }

    #[test]
    fn oversized_background_image_dropped_by_budget() {
        let mut root = boxes_of(r#"<div style="background-image:url(big.png)">x</div>"#, "");
        // simulate a probed header over the separate bg budget (4M px > 256K)
        root.children[0].children[0].img = Some(ImgSlot::Probed {
            src: std::rc::Rc::from("big.png"),
            w: 2000,
            h: 2000,
        });
        let l = layout_tree(&mut root, 500.0, &m4);
        assert!(l.images.is_empty(), "over-budget bg never decodes/draws");
        assert_eq!(root.children[0].children[0].w, 0.0, "slot de-sized");
    }

    // ---- background-size / background-position / background-repeat ----

    /// Blank decoded image for bg-paint tests (the renderer scales whatever
    /// buffer the slot carries — pixels are irrelevant to geometry).
    fn blank_img(w: u32, h: u32) -> std::rc::Rc<crate::image::ImageData> {
        std::rc::Rc::new(crate::image::ImageData {
            w,
            h,
            px: vec![0; (w * h) as usize],
        })
    }

    /// Full-width box with one background placeholder whose slot carries an
    /// image of intrinsic (iw, ih) — `ready` = decoded (paints), else
    /// probed (geometry only).
    fn bg_box(css: &str, iw: u32, ih: u32, ready: bool) -> BoxNode {
        let mut root = boxes_of(&format!(r#"<div style="{css}">x</div>"#), "");
        let ph = &mut root.children[0].children[0];
        assert!(ph.is_bg, "placeholder is the first child");
        ph.img = Some(if ready {
            ImgSlot::Ready(blank_img(iw, ih))
        } else {
            ImgSlot::Probed {
                src: std::rc::Rc::from("t.png"),
                w: iw,
                h: ih,
            }
        });
        root
    }

    #[test]
    fn background_size_percent_and_explicit_scale_placeholder() {
        // `50%` = half the padding-box width; the missing height follows
        // the intrinsic aspect ratio (20×10 → 100×50 in a 200px box)
        let mut root = bg_box(
            "width:200px;min-height:100px;background-image:url(t.png);\
             background-size:50%;background-repeat:no-repeat",
            20,
            10,
            false,
        );
        layout_tree(&mut root, 500.0, &m4);
        let bg = &root.children[0].children[0];
        assert_eq!((bg.w, bg.h), (100.0, 50.0), "50% width, aspect height");
        assert_eq!((bg.x, bg.y), (0.0, 0.0), "0% 0% default position");
        assert_eq!(bg.bg_tile_region, None, "no-repeat records no region");
        // explicit two-value pair sets both axes (px lengths)
        let mut root = bg_box(
            "width:200px;min-height:100px;background-image:url(t.png);\
             background-size:60px 30px;background-repeat:no-repeat",
            20,
            10,
            false,
        );
        layout_tree(&mut root, 500.0, &m4);
        let bg = &root.children[0].children[0];
        assert_eq!((bg.w, bg.h), (60.0, 30.0), "explicit width height");
    }

    #[test]
    fn background_cover_contain_fit_box_preserving_aspect() {
        // box 200×100, intrinsic 40×40: cover scales to max(200/40, 100/40)
        // = 5 → 200×200 (cropped); contain to min = 2.5 → 100×100 (fits)
        for (val, want) in [("cover", (200.0, 200.0)), ("contain", (100.0, 100.0))] {
            let mut root = bg_box(
                &format!(
                    "width:200px;min-height:100px;background-image:url(t.png);\
                     background-size:{val};background-repeat:no-repeat"
                ),
                40,
                40,
                false,
            );
            layout_tree(&mut root, 500.0, &m4);
            let bg = &root.children[0].children[0];
            assert_eq!((bg.w, bg.h), want, "{} keeps aspect", val);
        }
    }

    #[test]
    fn background_position_percent_keywords_lengths() {
        // box 100×50, intrinsic 10×10
        let geom = |pos: &str| {
            let mut root = bg_box(
                &format!(
                    "width:100px;min-height:50px;background-image:url(t.png);\
                     background-position:{pos};background-repeat:no-repeat"
                ),
                10,
                10,
                false,
            );
            layout_tree(&mut root, 500.0, &m4);
            let bg = &root.children[0].children[0];
            (bg.x, bg.y)
        };
        // 100% 100%: (box − image) × 1 → flush bottom-right
        assert_eq!(geom("100% 100%"), (90.0, 40.0));
        // `center` = 50% 50% → the (100−10)/2 = 45, (50−10)/2 = 20 midpoint
        assert_eq!(geom("center"), (45.0, 20.0));
        // lengths offset from the padding-box edge verbatim
        assert_eq!(geom("10px 5px"), (10.0, 5.0));
    }

    #[test]
    fn background_repeat_declared_tiles_paint_entries() {
        // box 50×30, 10×10 tiles: repeat = 5×3 entries over the padding box
        for (rep, want) in [("repeat", 15), ("repeat-x", 5), ("repeat-y", 3), ("no-repeat", 1)] {
            let mut root = bg_box(
                &format!(
                    "width:50px;min-height:30px;background-image:url(t.png);\
                     background-repeat:{rep}"
                ),
                10,
                10,
                true,
            );
            let l = layout_tree(&mut root, 500.0, &m4);
            assert_eq!(l.images.len(), want, "{rep} tile count");
            assert_eq!(
                (l.images[0].0, l.images[0].1, l.images[0].2, l.images[0].3),
                (0.0, 0.0, 10.0, 10.0),
                "{rep} first tile at the 0% 0% origin"
            );
        }
        // the tiling region rides the laid-out placeholder for repaints
        let mut root = bg_box(
            "width:50px;min-height:30px;background-image:url(t.png);background-repeat:repeat",
            10,
            10,
            true,
        );
        layout_tree(&mut root, 500.0, &m4);
        assert_eq!(
            root.children[0].children[0].bg_tile_region,
            Some((0.0, 0.0, 50.0, 30.0)),
            "region = the used padding box"
        );
    }

    #[test]
    fn background_shorthand_geometry_reaches_layout() {
        // `center/cover` through the shorthand: box 100×50, intrinsic 40×10
        // → cover scale max(2.5, 5) = 5 → 200×50, centered → x = −50
        let mut root = bg_box(
            "width:100px;min-height:50px;background:url(t.png) center/cover no-repeat",
            40,
            10,
            true,
        );
        let l = layout_tree(&mut root, 500.0, &m4);
        let bg = &root.children[0].children[0];
        assert_eq!((bg.w, bg.h), (200.0, 50.0), "shorthand cover");
        assert_eq!((bg.x, bg.y), (-50.0, 0.0), "shorthand center position");
        assert_eq!(l.images.len(), 1, "shorthand no-repeat draws once");
    }

    #[test]
    fn background_undeclared_geometry_keeps_today_geometry() {
        // regression guard: `background-image` alone (none of the new
        // properties declared) keeps the historical geometry EXACTLY —
        // padding-box origin (border-box inset by the border widths), the
        // padding INSIDE it, intrinsic size, ONE draw entry, no tiling
        let mut root = bg_box(
            "padding:10px;border:2px solid #000;background-image:url(t.png)",
            2,
            1,
            true,
        );
        let l = layout_tree(&mut root, 500.0, &m4);
        let bg = &root.children[0].children[0];
        assert_eq!((bg.x, bg.y), (2.0, 2.0), "padding-box top-left");
        assert_eq!((bg.w, bg.h), (2.0, 1.0), "intrinsic size");
        assert_eq!(bg.bg_tile_region, None, "undeclared repeat never tiles");
        assert_eq!(l.images.len(), 1, "drawn exactly once");
        assert_eq!(
            (l.images[0].0, l.images[0].1, l.images[0].2, l.images[0].3),
            (2.0, 2.0, 2.0, 1.0)
        );
    }

    // ---- mask-image (mask placeholder + background replacement) ----

    #[test]
    fn mask_image_builds_flagged_placeholder_and_flows_nothing() {
        let mut root = boxes_of(
            r#"<div style="background-image:url(b.png);mask-image:url(m.png)">x</div>"#,
            "",
        );
        let div = &root.children[0];
        assert_eq!(
            div.children.len(),
            3,
            "bg placeholder + mask placeholder + the text run"
        );
        assert!(div.children[0].is_bg && !div.children[0].is_mask);
        let m = &div.children[1];
        assert!(m.is_mask && !m.is_bg, "second child is the mask placeholder");
        match &m.img {
            Some(ImgSlot::Pending(src)) => assert_eq!(&**src, "m.png"),
            _ => panic!("the mask rides the shared <img> pipeline as a Pending slot"),
        }
        // the mask geometry longhands ride onto the placeholder (place()
        // reads them through the bg_* fields) — undeclared stays initial
        assert_eq!(m.bx.bg_size, BgSize::Auto);
        assert_eq!(m.bx.bg_repeat, None);
        // out of flow like the bg placeholder: reserves no space, so the
        // box's height matches the same box with no mask at all
        let mut with_mask = boxes_of(
            r#"<div style="width:200px;mask-image:url(m.png)">hello</div>"#,
            "",
        );
        let mut without = boxes_of(r#"<div style="width:200px">hello</div>"#, "");
        let a = layout_tree(&mut with_mask, 500.0, &m4);
        let b = layout_tree(&mut without, 500.0, &m4);
        assert_eq!(
            a.height, b.height,
            "the mask placeholder contributes no flow space"
        );
    }

    #[test]
    fn decoded_mask_replaces_background_and_tints_with_bg_color() {
        // bg-color + decoded mask: NO bg-color rect, and the mask image is
        // the only draw entry — its RGB replaced by the bg color, alpha
        // kept (the 0x80 alpha here must survive), cached over the slot so
        // paint reuses the tinted buffer instead of re-tinting per frame
        let mut root = boxes_of(
            r#"<div style="width:60px;min-height:40px;background:#ff8000;mask-image:url(m.png)">x</div>"#,
            "",
        );
        let div = &mut root.children[0];
        let m = div
            .children
            .iter_mut()
            .find(|c| c.is_mask)
            .expect("mask child");
        m.img = Some(ImgSlot::Ready(std::rc::Rc::new(crate::image::ImageData {
            w: 2,
            h: 2,
            px: vec![0x80CC0000; 4],
        })));
        let l = layout_tree(&mut root, 500.0, &m4);
        assert!(
            l.rects.is_empty(),
            "the bg-color rect is suppressed by the mask"
        );
        assert_eq!(l.images.len(), 1, "the mask image is the only draw entry");
        assert_eq!(
            l.images[0].4.px[0],
            0x80FF8000,
            "RGB replaced by the bg color (#ff8000), alpha kept"
        );
        // tint cached in the placeholder's Ready slot — the entry reuses
        // that exact buffer (one tint, no per-frame re-tint, nothing leaked)
        let slot = root.children[0]
            .children
            .iter()
            .find(|c| c.is_mask)
            .expect("mask child");
        match &slot.img {
            Some(ImgSlot::Ready(d)) => {
                assert_eq!(d.px[0], 0x80FF8000, "tinted pixels cached in the slot");
                assert!(
                    std::rc::Rc::ptr_eq(d, &l.images[0].4),
                    "the entry IS the cached Rc"
                );
            }
            _ => panic!("the slot stays Ready after the tint pass"),
        }
    }

    #[test]
    fn mask_without_background_color_paints_untinted() {
        let mut root = boxes_of(r#"<div style="mask-image:url(m.png)">x</div>"#, "");
        let div = &mut root.children[0];
        let m = div
            .children
            .iter_mut()
            .find(|c| c.is_mask)
            .expect("mask child");
        m.img = Some(ImgSlot::Ready(std::rc::Rc::new(crate::image::ImageData {
            w: 2,
            h: 2,
            px: vec![0x80CC0000; 4],
        })));
        let l = layout_tree(&mut root, 500.0, &m4);
        assert_eq!(l.images.len(), 1, "the untinted mask still draws once");
        assert_eq!(
            l.images[0].4.px[0], 0x80CC0000,
            "no bg color on the box → the mask keeps its own pixels"
        );
        assert!(l.rects.is_empty(), "nothing else to paint");
    }

    #[test]
    fn mask_size_halves_the_drawn_tile() {
        // `mask-size: 50%` on the 200px padding box with a 20×10 intrinsic
        // → a 100×50 tile, `center` offsets it (50, 25) — the same math
        // background-size gets, against the padding box
        let css = "width:200px;min-height:100px;mask-image:url(m.png);\
                   mask-size:50%;mask-position:center;mask-repeat:no-repeat";
        let mut root = boxes_of(&format!(r#"<div style="{css}">x</div>"#), "");
        let div = &mut root.children[0];
        let m = div
            .children
            .iter_mut()
            .find(|c| c.is_mask)
            .expect("mask child");
        m.img = Some(ImgSlot::Ready(blank_img(20, 10)));
        let l = layout_tree(&mut root, 500.0, &m4);
        let m = root.children[0]
            .children
            .iter()
            .find(|c| c.is_mask)
            .expect("mask child");
        assert_eq!(
            (m.w, m.h),
            (100.0, 50.0),
            "50% of the padding box, height follows the aspect ratio"
        );
        assert_eq!((m.x, m.y), (50.0, 25.0), "centered in the padding box");
        assert_eq!(l.images.len(), 1, "no-repeat draws the tile once");
        assert_eq!(
            (l.images[0].0, l.images[0].1, l.images[0].2, l.images[0].3),
            (50.0, 25.0, 100.0, 50.0),
            "the entry is the halved tile at its centered spot"
        );
    }

    #[test]
    fn undecoded_mask_paints_nothing() {
        // the slot never fills (fetch failed / decode not run yet): the
        // mask-only box paints NOTHING — no image entry, no rect — while
        // its text flows as usual
        let mut root = boxes_of(r#"<div style="mask-image:url(m.png)">x</div>"#, "");
        let l = layout_tree(&mut root, 500.0, &m4);
        assert!(l.images.is_empty(), "no decoded mask, no draw entry");
        assert!(l.rects.is_empty(), "no bg color either — nothing to paint");
        assert!(!l.runs.is_empty(), "text unaffected");
        // with a bg color the suppression holds while the mask is only
        // Pending (production lays out BEFORE it decodes): the rect the box
        // would have drawn is gone too — the mask entry arrives at decode
        let mut root = boxes_of(
            r#"<div style="background:#3366ff;mask-image:url(m.png)">x</div>"#,
            "",
        );
        let l = layout_tree(&mut root, 500.0, &m4);
        assert!(
            l.rects.is_empty(),
            "bg-color rect suppressed while the mask is undecoded"
        );
        assert!(l.images.is_empty(), "and the mask draws nothing yet");
    }

    #[test]
    fn box_without_mask_emits_exact_paint_entries() {
        // regression guard: a box with no mask property emits exactly the
        // entries it always has — the bg-color rect, the four border strips
        // in their fixed order, and one bg image at the padding-box origin
        let css = "width:100px;min-height:60px;background:#3366ff;\
                   border:2px solid #000;background-image:url(t.png);\
                   background-repeat:no-repeat";
        let mut root = boxes_of(&format!(r#"<div style="{css}">x</div>"#), "");
        root.children[0].children[0].img = Some(ImgSlot::Ready(blank_img(10, 10)));
        let l = layout_tree(&mut root, 500.0, &m4);
        let rects: Vec<(f32, f32, f32, f32, u32)> = l
            .rects
            .iter()
            .map(|r| (r.0, r.1, r.2, r.3, r.4))
            .collect();
        assert_eq!(
            rects,
            vec![
                (0.0, 0.0, 100.0, 64.0, 0x3366ff), // bg-color rect
                (0.0, 0.0, 100.0, 2.0, 0x000000),  // strips: top
                (0.0, 62.0, 100.0, 2.0, 0x000000), // bottom
                (0.0, 2.0, 2.0, 60.0, 0x000000),   // left
                (98.0, 2.0, 2.0, 60.0, 0x000000),  // right
            ],
            "rect channel byte-identical to the pre-mask build"
        );
        let imgs: Vec<(f32, f32, f32, f32)> = l
            .images
            .iter()
            .map(|i| (i.0, i.1, i.2, i.3))
            .collect();
        assert_eq!(
            imgs,
            vec![(2.0, 2.0, 10.0, 10.0)],
            "bg image entry byte-identical too"
        );
    }

    // ---- table percentage widths / presentational attributes ----

    #[test]
    fn table_width_attr_percent_matches_css() {
        // <table width="85%"> behaves exactly like `table { width: 85% }`
        let mut by_attr =
            boxes_of(r#"<table width="85%"><tr><td>x</td></tr></table>"#, "");
        let mut by_css =
            boxes_of(r#"<table><tr><td>x</td></tr></table>"#, "table { width: 85% }");
        let la = layout_tree(&mut by_attr, 1000.0, &m4);
        let lc = layout_tree(&mut by_css, 1000.0, &m4);
        assert_eq!(by_attr.children[0].w, 850.0, "85% of the 1000px container");
        assert_eq!(by_css.children[0].w, 850.0, "CSS width resolves the same");
        assert_eq!(la.height, lc.height, "same layout either way");
        // px values too
        let mut px = boxes_of(r#"<table width="400"><tr><td>x</td></tr></table>"#, "");
        layout_tree(&mut px, 1000.0, &m4);
        assert_eq!(px.children[0].w, 400.0, "width=\"400\" = 400px");
    }

    #[test]
    fn table_align_center_centers_band_and_css_wins() {
        let mut root = boxes_of(
            r#"<table width="85%" align="center"><tr><td>x</td></tr></table>"#,
            "",
        );
        layout_tree(&mut root, 1000.0, &m4);
        let t = &root.children[0];
        assert_eq!(t.w, 850.0);
        assert_eq!(t.x, 75.0, "centered in the 1000px container");
        // author CSS beats both hints (declared width + explicit margin)
        let mut styled = boxes_of(
            r#"<table width="85%" align="center"><tr><td>x</td></tr></table>"#,
            "table { width: 400px; margin-left: 10px }",
        );
        layout_tree(&mut styled, 1000.0, &m4);
        let t = &styled.children[0];
        assert_eq!(t.w, 400.0, "author width wins over the attribute");
        assert_eq!(t.x, 10.0, "author margin wins over align=center");
    }

    // ---- typography: text-transform / letter-spacing / word-spacing /
    // white-space / text-indent ----

    #[test]
    fn text_transform_applies_when_text_boxes_are_built() {
        // the transform lands on the BoxNode text itself, so runs, wrapping
        // and draw all see the final string (nothing downstream re-cases)
        let mut up = boxes_of(r#"<div style="text-transform:uppercase">abc</div>"#, "");
        assert_eq!(
            up.children[0].children[0].text, "ABC",
            "box text transformed at build time"
        );
        let l = layout_tree(&mut up, 500.0, &m4);
        assert_eq!(l.runs[0].text, "ABC", "run carries the transformed text");

        let mut low = boxes_of(r#"<div style="text-transform:lowercase">AbC</div>"#, "");
        let l = layout_tree(&mut low, 500.0, &m4);
        assert_eq!(l.runs[0].text, "abc");

        // capitalize = first ASCII letter of each whitespace-separated word,
        // the rest of the word keeps its authored case
        let mut cap = boxes_of(r#"<div style="text-transform:capitalize">hello bIG world</div>"#, "");
        let l = layout_tree(&mut cap, 500.0, &m4);
        let texts: Vec<&str> = l.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["Hello", "BIG", "World"]);
    }

    #[test]
    fn letter_spacing_widens_run_and_wraps_earlier() {
        // run width = measure + spacing × char count (spec: a space after
        // EVERY glyph, incl. the last — 4 glyphs × 4px on top of 16px)
        let mut root = boxes_of(r#"<div style="letter-spacing:4px">abcd</div>"#, "");
        let l = layout_tree(&mut root, 500.0, &m4);
        assert_eq!(l.runs[0].width, 16.0 + 4.0 * 4.0, "spacing × count added");

        // same 24px box: plain "aa bb" = 8 + 4 + 8 = 20 fits one line …
        let mut plain = boxes_of(r#"<div style="width:24px">aa bb</div>"#, "");
        let lp = layout_tree(&mut plain, 500.0, &m4);
        assert_eq!(lp.runs.len(), 2);
        assert_eq!(lp.runs[0].y, lp.runs[1].y, "plain text fits on one line");

        // … tracked: "aa" = 8 + 8 = 16, the space = 4 + 4 = 8 → the cursor
        // sits AT the 24px edge before "bb" even starts → it must wrap
        let mut tracked =
            boxes_of(r#"<div style="letter-spacing:4px; width:24px">aa bb</div>"#, "");
        let lt = layout_tree(&mut tracked, 500.0, &m4);
        assert_eq!(lt.runs.len(), 2);
        assert!(lt.runs[1].y > lt.runs[0].y, "letter-spacing wraps earlier");
        assert_eq!(lt.runs[0].width, 16.0, "aa = 8px + 4×2");
    }

    #[test]
    fn word_spacing_adds_delta_to_every_inter_word_space() {
        let mut root = boxes_of(r#"<div style="word-spacing:10px">a b c</div>"#, "");
        let l = layout_tree(&mut root, 500.0, &m4);
        assert_eq!(l.runs.len(), 3, "collapsed spaces are not runs");
        // each gap = one collapsed space (4px) + the 10px delta, on BOTH
        // spaces (layout line builder and draw_text share spacing_extra)
        let gap1 = l.runs[1].x - (l.runs[0].x + l.runs[0].width);
        let gap2 = l.runs[2].x - (l.runs[1].x + l.runs[1].width);
        assert_eq!(gap1, 14.0, "space 4px + word-spacing 10px");
        assert_eq!(gap2, 14.0, "same delta on the second space");
        // the shared rule counts every whitespace char of the string
        let st = Style {
            word_spacing: 10.0,
            ..Style::new()
        };
        assert_eq!(spacing_extra("a b c", &st), 20.0, "2 spaces × 10px");
    }

    #[test]
    fn white_space_pre_preserves_double_spaces_and_breaks_at_newline() {
        // html.rs never folds text, so the raw "a  b\nc" reaches the box
        // tree; the UA Ws::Pre on <pre> (style_for) is what keeps it — real
        // newline below, double space intact, one hard break between lines
        let mut root = boxes_of("<pre>a  b\nc</pre>", "");
        let l = layout_tree(&mut root, 500.0, &m4);
        assert_eq!(l.runs.len(), 2, "one run per pre line");
        assert_eq!(l.runs[0].text, "a  b", "double space preserved");
        assert_eq!(l.runs[0].width, 16.0, "4 chars × 4px (folded would be 12)");
        assert_eq!(l.runs[1].text, "c");
        assert!(l.runs[1].y > l.runs[0].y, "\\n became a hard break");

        // explicit `normal` still folds (and ignores \n like it always did)
        let mut n = boxes_of(r#"<div style="white-space:normal">aa  bb</div>"#, "");
        let ln = layout_tree(&mut n, 500.0, &m4);
        assert_eq!(ln.runs.len(), 2, "double space collapses to one separator");
        assert_eq!(ln.runs[1].x, 12.0, "aa(8) + ONE space(4)");
    }

    #[test]
    fn white_space_nowrap_keeps_line_wider_than_box_unclipped() {
        // 10px box fits only "aa"; nowrap keeps both words on one line
        // (a run may exceed the box — clipping is overflow's job, and no
        // overflow is declared here)
        let mut root = boxes_of(r#"<div style="white-space:nowrap; width:10px">aa bb</div>"#, "");
        let l = layout_tree(&mut root, 500.0, &m4);
        assert_eq!(l.runs.len(), 2);
        assert_eq!(l.runs[0].y, l.runs[1].y, "nowrap never wraps");
        assert!(
            l.runs[1].x + l.runs[1].width > 10.0,
            "run extends past the 10px box (unclipped)"
        );
        assert!(l.runs.iter().all(|r| r.clip.is_none()), "no clip without overflow");

        // the same box with normal white-space wraps at the same width
        let mut plain = boxes_of(r#"<div style="width:10px">aa bb</div>"#, "");
        let lp = layout_tree(&mut plain, 500.0, &m4);
        assert!(lp.runs[1].y > lp.runs[0].y, "normal wraps at 10px");
    }

    #[test]
    fn text_indent_first_line_only_negative_and_absent() {
        // first line starts 20px in; the WRAPPED line returns to x=0
        let mut root =
            boxes_of(r#"<div style="width:60px; text-indent:20px">aa bb cc dd</div>"#, "");
        let l = layout_tree(&mut root, 500.0, &m4);
        assert_eq!(l.runs[0].x, 20.0, "first line starts at the indent");
        assert!(l.runs[3].y > l.runs[0].y, "text wrapped to a second line");
        assert_eq!(l.runs[3].x, 0.0, "second line ignores text-indent");

        // negative indent shifts the first line LEFT of the box edge
        let mut neg = boxes_of(r#"<div style="width:100px; text-indent:-10px">aa bb</div>"#, "");
        let ln = layout_tree(&mut neg, 500.0, &m4);
        assert_eq!(ln.runs[0].x, -10.0, "negative indent shifts left");

        // undeclared → initial 0: first run at the box edge
        let mut none = boxes_of(r#"<div style="width:100px">aa bb</div>"#, "");
        let l0 = layout_tree(&mut none, 500.0, &m4);
        assert_eq!(l0.runs[0].x, 0.0, "no text-indent → x=0");
    }

    #[test]
    fn no_typography_property_declared_lays_out_byte_identically() {
        // regression guard: a fixture declaring NONE of text-transform /
        // letter-spacing / word-spacing / white-space / text-indent must
        // lay out byte-identically — the run dump below is the pre-batch
        // geometry (the double space AND the \n prove normal still folds
        // whitespace and still ignores newlines)
        let mut root = boxes_of(
            "<div style=\"width:40px\">aa bb  cc\ndd</div>",
            "div { color: #123456 }",
        );
        let l = layout_tree(&mut root, 500.0, &m4);
        let dump: Vec<(String, f32, f32, f32)> = l
            .runs
            .iter()
            .map(|r| (r.text.clone(), r.x, r.y, r.width))
            .collect();
        let want: Vec<(String, f32, f32, f32)> = vec![
            ("aa".into(), 0.0, 0.0, 8.0),
            ("bb".into(), 12.0, 0.0, 8.0),
            ("cc".into(), 24.0, 0.0, 8.0),
            ("dd".into(), 0.0, 15.0 * 1.35, 8.0),
        ];
        assert_eq!(
            dump, want,
            "undeclared typography must not move a single pixel"
        );
    }

    // ---- list markers (list-style-*, ::marker) ----

    #[test]
    fn ul_marker_hangs_left_of_its_text() {
        let mut root = tree_of("<ul><li>alpha</li></ul>");
        let l = layout_tree(&mut root, 400.0, &m4);
        let marker = l.runs.iter().find(|r| r.text == "\u{2022}").expect("disc marker run");
        let text = l.runs.iter().find(|r| r.text == "alpha").expect("item text");
        assert_eq!(marker.y, text.y, "marker sits on the item's first line");
        assert!(marker.x < text.x, "marker left of the text: {} vs {}", marker.x, text.x);
        assert!(
            marker.x + marker.width <= text.x,
            "marker clears the text: {} + {} > {}",
            marker.x,
            marker.width,
            text.x
        );
        // the UA 40px list padding puts the item text at x=40
        assert_eq!(text.x, 40.0, "list content starts after the UA gutter");
    }

    #[test]
    fn ol_numbers_three_items_and_counts_per_list() {
        let mut root = tree_of("<ol><li>a</li><li>b</li><li>c</li></ol>");
        let l = layout_tree(&mut root, 400.0, &m4);
        let markers: Vec<&str> = l
            .runs
            .iter()
            .filter(|r| r.text.ends_with('.'))
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(markers, vec!["1.", "2.", "3."]);
        // each marker is on its item's line
        let a = l.runs.iter().find(|r| r.text == "a").unwrap();
        let c = l.runs.iter().find(|r| r.text == "c").unwrap();
        assert!(c.y > a.y, "items stack");
        assert_eq!(
            l.runs.iter().filter(|r| r.text.ends_with('.')).count(),
            3,
            "exactly one marker per item"
        );
    }

    #[test]
    fn ol_start_and_li_value_seed_the_counter() {
        let mut root =
            tree_of(r#"<ol start="5"><li>a</li><li value="9">b</li><li>c</li></ol>"#);
        let l = layout_tree(&mut root, 400.0, &m4);
        let markers: Vec<&str> = l
            .runs
            .iter()
            .filter(|r| r.text.ends_with('.'))
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(markers, vec!["5.", "9.", "10."], "start= and value= both honored");
    }

    #[test]
    fn nested_list_restarts_its_own_counter() {
        let mut root = tree_of("<ol><li>a<ol><li>b</li></ol></li></ol>");
        let l = layout_tree(&mut root, 400.0, &m4);
        let ones: Vec<&PositionedRun> =
            l.runs.iter().filter(|r| r.text == "1.").collect();
        assert_eq!(ones.len(), 2, "outer and inner lists each count from 1");
        assert!(
            ones[1].x > ones[0].x,
            "nested marker indented by the nested list: {} vs {}",
            ones[1].x,
            ones[0].x
        );
    }

    #[test]
    fn list_style_type_none_suppresses_the_marker() {
        for css in ["li { list-style-type: none }", "li { list-style: none }"] {
            let mut root = boxes_of("<ul><li>a</li><li>b</li></ul>", css);
            let l = layout_tree(&mut root, 400.0, &m4);
            assert_eq!(
                l.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
                vec!["a", "b"],
                "no marker runs with {css}"
            );
        }
    }

    #[test]
    fn ordered_marker_strings_alpha_and_roman() {
        let mut alpha = boxes_of(
            "<ol><li>a</li><li>b</li><li>c</li></ol>",
            "ol { list-style-type: upper-alpha }",
        );
        let la = layout_tree(&mut alpha, 400.0, &m4);
        let am: Vec<&str> = la.runs.iter().filter(|r| r.text.ends_with('.')).map(|r| r.text.as_str()).collect();
        assert_eq!(am, vec!["A.", "B.", "C."]);

        let mut roman = boxes_of(
            "<ol><li>a</li><li>b</li><li>c</li><li>d</li></ol>",
            "ol { list-style-type: lower-roman }",
        );
        let lr = layout_tree(&mut roman, 400.0, &m4);
        let rm: Vec<&str> = lr.runs.iter().filter(|r| r.text.ends_with('.')).map(|r| r.text.as_str()).collect();
        assert_eq!(rm, vec!["i.", "ii.", "iii.", "iv."]);
    }

    #[test]
    fn circle_and_square_glyphs_replace_the_disc() {
        let mut circle = boxes_of("<ul><li>a</li></ul>", "ul { list-style-type: circle }");
        let lc = layout_tree(&mut circle, 400.0, &m4);
        assert!(lc.runs.iter().any(|r| r.text == "\u{25e6}"), "circle glyph");
        let mut square = boxes_of("<ul><li>a</li></ul>", "ul { list-style-type: square }");
        let ls = layout_tree(&mut square, 400.0, &m4);
        assert!(ls.runs.iter().any(|r| r.text == "\u{25aa}"), "square glyph");
    }

    #[test]
    fn inside_marker_leads_the_first_line_in_flow() {
        let mut root = boxes_of(
            "<ul><li>alpha</li></ul>",
            "ul { list-style-position: inside }",
        );
        let l = layout_tree(&mut root, 400.0, &m4);
        let marker = l.runs.iter().find(|r| r.text == "\u{2022}").expect("marker run");
        let text = l.runs.iter().find(|r| r.text == "alpha").expect("text run");
        assert_eq!(marker.x, 40.0, "marker starts at the item's text start");
        assert_eq!(marker.y, text.y, "text follows on the same line");
        assert!(text.x > marker.x, "text after the in-flow marker");
        assert!(
            text.x <= marker.x + marker.width + 10.0,
            "no gutter gap for inside markers"
        );
    }

    #[test]
    fn marker_pseudo_content_and_color_override_the_default() {
        let mut root = boxes_of(
            "<ul><li>a</li></ul>",
            "li::marker { content: \"X\"; color: #ff0000 }",
        );
        let l = layout_tree(&mut root, 400.0, &m4);
        let m = l
            .runs
            .iter()
            .find(|r| r.text == "X")
            .expect("::marker content replaces the bullet");
        assert_eq!(m.style.color, 0x00_ff_00_00, "::marker color applies");
        assert!(!l.runs.iter().any(|r| r.text == "\u{2022}"), "bullet gone");
        // counter(list-item) in ::marker resolves to the item's number
        let mut root2 = boxes_of(
            "<ol><li>a</li><li>b</li></ol>",
            "li::marker { content: counter(list-item) }",
        );
        let l2 = layout_tree(&mut root2, 400.0, &m4);
        let nums: Vec<&str> = l2.runs.iter().filter(|r| r.text.ends_with('.')).map(|r| r.text.as_str()).collect();
        assert_eq!(nums, vec!["1.", "2."]);
    }

    #[test]
    fn plain_paragraph_layout_is_untouched_by_list_support() {
        let mut root = boxes_of("<p>plain text</p>", "");
        let l = layout_tree(&mut root, 400.0, &m4);
        let dump: Vec<(String, f32, f32)> = l
            .runs
            .iter()
            .map(|r| (r.text.clone(), r.x, r.y))
            .collect();
        assert_eq!(
            dump,
            vec![("plain".into(), 0.0, 0.0), ("text".into(), 24.0, 0.0)],
            "a plain <p> keeps its flush-left geometry"
        );
    }
}
