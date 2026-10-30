//! Borderless chrome: a custom titlebar (tab strip + window controls + drag
//! region) and a toolbar row (back button + address pill), painted from raw
//! egui shapes so the window can drop the native frame entirely. Every color
//! and size lives here as a design token; `main.rs` only translates the
//! returned events into navigation.

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui,
};

// ---------------------------------------------------------------------------
// design tokens
// ---------------------------------------------------------------------------

/// Window background, shared by both chrome rows.
pub const CHROME_BG: Color32 = Color32::from_rgb(0x1b, 0x1c, 0x1f);
/// Toolbar/page boundary line.
const SEP_PAGE: Color32 = Color32::from_rgb(0x2c, 0x2d, 0x31);
/// Tab strip/toolbar boundary line (a touch quieter than the page one).
const SEP_ROWS: Color32 = Color32::from_rgb(0x26, 0x27, 0x2b);

/// Titlebar height — the tab strip lives inside it.
pub const TAB_H: f32 = 40.0;
/// Toolbar row height.
pub const TOOLBAR_H: f32 = 44.0;

const TAB_BODY_H: f32 = 30.0;
const TAB_W_MIN: f32 = 60.0;
const TAB_W_MAX: f32 = 180.0;
const TAB_PAD: f32 = 10.0;
/// Gutter reserved on the right of every tab for the close ✕.
const CLOSE_GUTTER: f32 = 18.0;
const TAB_GAP: f32 = 4.0;
const STRIP_X0: f32 = 8.0;

const TAB_FG: Color32 = Color32::from_rgb(0xc9, 0xcd, 0xd2);
const FG_HOVER: Color32 = Color32::from_rgb(0xe8, 0xea, 0xed);
const FG_ACTIVE: Color32 = Color32::WHITE;
const BG_HOVER: Color32 = Color32::from_rgb(0x2a, 0x2b, 0x2f);
const BG_ACTIVE: Color32 = Color32::from_rgb(0x35, 0x36, 0x3b);
const ICON: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xa6);
const ICON_HOVER: Color32 = FG_HOVER;
const WIN_FG: Color32 = FG_HOVER;
const WIN_CLOSE_BG: Color32 = Color32::from_rgb(0xe8, 0x11, 0x23);

const PILL_H: f32 = 28.0;
const PILL_RADIUS: f32 = 10.0;
const PILL_X0: f32 = 52.0;
const PILL_X1: f32 = 12.0;
const PILL_PAD: f32 = 12.0;
const PILL_BG: Color32 = BG_HOVER;
const PILL_RING: Color32 = Color32::from_rgb(0x6a, 0x9c, 0xff);
const PILL_FG: Color32 = FG_HOVER;
const PILL_HINT: Color32 = Color32::from_rgb(0x8b, 0x90, 0x96);
const DISABLED_FG: Color32 = Color32::from_rgb(0x5f, 0x63, 0x68);

const TEXT: f32 = 13.0;
const NEW_TAB_SIZE: f32 = 24.0;
const BACK_SIZE: f32 = 30.0;
const WIN_W: f32 = 46.0;
const WIN_ICON: f32 = 6.0; // window-control glyph half-box (12px glyphs)

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// What the tab strip wants `main.rs` to do.
pub enum TabEvent {
    Activate(usize),
    Close(usize),
    NewTab,
}

/// What the toolbar wants `main.rs` to do. `Focused` is emitted every frame.
pub enum ToolbarEvent {
    Back,
    Submit(String),
    Focused(bool),
}

// ---------------------------------------------------------------------------
// tab strip (titlebar row)
// ---------------------------------------------------------------------------

/// Paint the titlebar row and report tab actions. Window controls and the
/// drag region act on the viewport directly (they need no App state).
pub fn tab_strip(ui: &mut Ui, titles: &[String], active: usize) -> Vec<TabEvent> {
    let mut events = Vec::new();
    let strip = ui.max_rect();
    let cy = strip.center().y;
    let font = FontId::proportional(TEXT);

    // quiet separator between the two chrome rows
    ui.painter().line_segment(
        [
            pos2(strip.left(), strip.bottom() - 0.5),
            pos2(strip.right(), strip.bottom() - 0.5),
        ],
        Stroke::new(1.0, SEP_ROWS),
    );

    // ---- tabs ----
    let mut x = strip.left() + STRIP_X0;
    for (i, title) in titles.iter().enumerate() {
        let is_active = i == active;
        // per-char advances give both the width and the ellipsis cut point
        let widths: Vec<f32> = title.chars().map(|c| measure(ui, &font, c)).collect();
        let ell_w = measure(ui, &font, '\u{2026}');
        let max_text = TAB_W_MAX - 2.0 * TAB_PAD - CLOSE_GUTTER;
        let keep = ellipsized_chars(&widths, max_text, ell_w);
        let ellipsized = keep < title.chars().count();
        let text_w: f32 =
            widths[..keep].iter().sum::<f32>() + if ellipsized { ell_w } else { 0.0 };
        let w = (text_w + 2.0 * TAB_PAD + CLOSE_GUTTER).clamp(TAB_W_MIN, TAB_W_MAX);
        let rect = Rect::from_center_size(pos2(x + w / 2.0, cy), vec2(w, TAB_BODY_H));
        x += w + TAB_GAP;

        let resp = ui.interact(rect, ui.scope_id().with(("chrome_tab", i)), Sense::click());
        let hovered = resp.hovered();        let bg = if is_active {
            BG_ACTIVE
        } else if hovered {
            BG_HOVER
        } else {
            Color32::TRANSPARENT
        };
        if bg != Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, 8.0, bg);
        }

        let fg = if is_active {
            FG_ACTIVE
        } else if hovered {
            FG_HOVER
        } else {
            TAB_FG
        };
        let shown: String = if ellipsized {
            let mut s: String = title.chars().take(keep).collect();
            s.push('\u{2026}');
            s
        } else {
            title.clone()
        };
        // title centered in the text area (i.e. left of the close gutter)
        let text_mid = rect.left() + TAB_PAD + (w - 2.0 * TAB_PAD - CLOSE_GUTTER) / 2.0;
        paint_label(ui, pos2(text_mid, cy), &shown, &font, fg, is_active);

        // close ✕ — only on the active or hovered tab
        if is_active || hovered {
            let c = pos2(rect.right() - TAB_PAD - 8.0, cy);
            let crect = Rect::from_center_size(c, vec2(16.0, 16.0));
            let cresp =
                ui.interact(crect, ui.scope_id().with(("chrome_tab_close", i)), Sense::click());
            let cicon = if cresp.hovered() { ICON_HOVER } else { ICON };
            paint_x(ui, c, 3.5, cicon);
            if cresp.clicked() {
                eprintln!("[chrome] tab {i} close");
                events.push(TabEvent::Close(i));
                continue; // the click was the close button's, not the tab's
            }
        }
        if resp.clicked() {
            eprintln!("[chrome] tab {i} activate");
            events.push(TabEvent::Activate(i));
        }
    }

    // ---- new-tab + ----
    let nb = Rect::from_center_size(
        pos2(x + NEW_TAB_SIZE / 2.0, cy),
        vec2(NEW_TAB_SIZE, NEW_TAB_SIZE),
    );
    let nresp = ui.interact(nb, ui.scope_id().with("chrome_new_tab"), Sense::click());
    if nresp.hovered() {
        ui.painter().rect_filled(nb, 6.0, BG_HOVER);
    }
    ui.painter().text(
        nb.center(),
        Align2::CENTER_CENTER,
        "+",
        FontId::proportional(15.0),
        if nresp.hovered() { ICON_HOVER } else { ICON },
    );
    if nresp.clicked() {
        eprintln!("[chrome] new tab");
        events.push(TabEvent::NewTab);
    }

    // ---- drag region + window controls ----
    let ctrls_l = strip.right() - 3.0 * WIN_W;
    let drag_l = nb.right() + 8.0;
    let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
    let drag_w = (ctrls_l - 8.0 - drag_l).max(0.0);
    if drag_w > 0.0 {
        let drect = Rect::from_min_size(pos2(drag_l, strip.top()), vec2(drag_w, strip.height()));
        // click_and_drag: plain clicks (no move) still count, so the
        // double-click can toggle maximize while a move starts a drag
        let dresp = ui.interact(drect, ui.scope_id().with("chrome_drag"), Sense::click_and_drag());
        if dresp.double_clicked() {
            eprintln!("[chrome] drag double-click -> maximize {maximized}");
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        } else if dresp.drag_started() {
            // OS-native window drag (titlebar behavior)
            eprintln!("[chrome] drag start");
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if dresp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if dresp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }

    for (k, ctl) in [WinCtl::Min, WinCtl::Max, WinCtl::Close]
        .into_iter()
        .enumerate()
    {
        let r = Rect::from_min_size(
            pos2(ctrls_l + k as f32 * WIN_W, strip.top()),
            vec2(WIN_W, strip.height()),
        );
        let resp = ui.interact(r, ui.scope_id().with(("chrome_winctl", k)), Sense::click());
        let close = matches!(ctl, WinCtl::Close);
        let (bg, fg) = match (close, resp.hovered()) {
            (true, true) => (WIN_CLOSE_BG, Color32::WHITE),
            (_, true) => (BG_HOVER, WIN_FG),
            _ => (Color32::TRANSPARENT, WIN_FG),
        };
        if bg != Color32::TRANSPARENT {
            ui.painter().rect_filled(r, 0.0, bg);
        }
        paint_win_icon(ui, r.center(), ctl, fg);
        if resp.clicked() {
            use egui::ViewportCommand as V;
            let cmd = match ctl {
                WinCtl::Min => {
                    eprintln!("[chrome] win minimize");
                    V::Minimized(true)
                }
                WinCtl::Max => {
                    eprintln!("[chrome] win maximize toggle (maximized={maximized})");
                    V::Maximized(!maximized)
                }
                WinCtl::Close => {
                    eprintln!("[chrome] win close");
                    V::Close
                }
            };
            ui.ctx().send_viewport_cmd(cmd);
        }
    }

    events
}

/// How many chars of a measured string still fit in `max_w` once an
/// ellipsis is appended. `widths` = per-char advances; the return value is
/// the number of chars to KEEP (excluding the ellipsis itself).
pub(crate) fn ellipsized_chars(widths: &[f32], max_w: f32, ellipsis_w: f32) -> usize {
    let total: f32 = widths.iter().sum();
    if total <= max_w {
        return widths.len();
    }
    let mut used = 0.0f32;
    let mut n = 0usize;
    for &w in widths {
        if used + w + ellipsis_w > max_w {
            break;
        }
        used += w;
        n += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// toolbar row
// ---------------------------------------------------------------------------

/// Paint the toolbar row (back button + address pill) and report actions.
pub fn toolbar(ui: &mut Ui, text: &mut String, can_go_back: bool) -> Vec<ToolbarEvent> {
    let mut events = Vec::new();
    let row = ui.max_rect();
    let cy = row.center().y;

    // chrome/page separator
    ui.painter().line_segment(
        [
            pos2(row.left(), row.bottom() - 0.5),
            pos2(row.right(), row.bottom() - 0.5),
        ],
        Stroke::new(1.0, SEP_PAGE),
    );

    // ---- back ----
    let brect =
        Rect::from_center_size(pos2(row.left() + 8.0 + BACK_SIZE / 2.0, cy), vec2(BACK_SIZE, BACK_SIZE));
    let bresp = ui.interact(brect, ui.scope_id().with("chrome_back"), Sense::click());
    let bfg = if !can_go_back {
        DISABLED_FG
    } else {
        if bresp.hovered() {
            ui.painter().rect_filled(brect, 6.0, BG_HOVER);
            ICON_HOVER
        } else {
            PILL_FG
        }
    };
    ui.painter().text(
        brect.center(),
        Align2::CENTER_CENTER,
        "\u{2039}", // ‹ — 22px so the angle reads at icon size
        FontId::proportional(22.0),
        bfg,
    );
    if can_go_back && bresp.clicked() {
        eprintln!("[chrome] back");
        events.push(ToolbarEvent::Back);
    }

    // ---- address pill ----
    let pill = Rect::from_center_size(
        pos2((row.left() + PILL_X0 + row.right() - PILL_X1) / 2.0, cy),
        vec2((row.right() - PILL_X1) - (row.left() + PILL_X0), PILL_H),
    );
    let presp = ui.allocate_rect(pill, Sense::click());
    ui.painter().rect_filled(pill, PILL_RADIUS, PILL_BG);

    let trect = Rect::from_min_max(
        pos2(pill.left() + PILL_PAD, cy - 9.0),
        pos2(pill.right() - PILL_PAD, cy + 9.0),
    );
    let te = egui::TextEdit::singleline(text)
        .id_salt("chrome_address")
        .frame(egui::Frame::NONE)
        .font(egui::TextStyle::Body)
        .text_color(PILL_FG)
        .margin(vec2(0.0, 0.0));
    let tresp = ui.put(trect, te);
    if tresp.gained_focus() {
        // click-to-focus keeps the URL and selects it all: typing replaces it
        eprintln!("[chrome] address focus (select all)");
        let mut state = egui::TextEdit::load_state(ui.ctx(), tresp.id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::select_all_str(text.as_str())));
        state.store(ui.ctx(), tresp.id);
    }
    if tresp.lost_focus() {
        eprintln!("[chrome] address blur");
    }
    if text.is_empty() && !tresp.has_focus() {
        ui.painter().text(
            pos2(pill.left() + PILL_PAD, cy),
            Align2::LEFT_CENTER,
            "Search or enter address",
            FontId::proportional(TEXT),
            PILL_HINT,
        );
    }
    if tresp.has_focus() || presp.has_focus() {
        ui.painter().rect_stroke(
            pill,
            PILL_RADIUS,
            Stroke::new(2.0, PILL_RING),
            StrokeKind::Outside,
        );
    }
    if presp.clicked() && !tresp.has_focus() {
        // clicked the pill padding → focus + select-all next frame
        tresp.request_focus();
    }

    if tresp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        let q = text.trim().to_string();
        if !q.is_empty() {
            eprintln!("[chrome] address submit: {q}");
            events.push(ToolbarEvent::Submit(q));
        }
    }
    events.push(ToolbarEvent::Focused(tresp.has_focus()));
    events
}

// ---------------------------------------------------------------------------
// painting helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum WinCtl {
    Min,
    Max,
    Close,
}

/// Advance width of one char in the given font.
fn measure(ui: &Ui, font: &FontId, c: char) -> f32 {
    ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(c.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
    })
}

/// Centered label. The default fonts ship no bold face, so the active tab is
/// faked with a hairline double-strike (reads as a semibold weight).
fn paint_label(ui: &Ui, at: Pos2, text: &str, font: &FontId, color: Color32, bold: bool) {
    ui.painter()
        .text(at, Align2::CENTER_CENTER, text, font.clone(), color);
    if bold {
        ui.painter().text(
            at + vec2(0.45, 0.0),
            Align2::CENTER_CENTER,
            text,
            font.clone(),
            color,
        );
    }
}

/// Small ✕ built from two strokes (no reliance on a glyph).
fn paint_x(ui: &Ui, c: Pos2, r: f32, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    ui.painter()
        .line_segment([pos2(c.x - r, c.y - r), pos2(c.x + r, c.y + r)], stroke);
    ui.painter()
        .line_segment([pos2(c.x + r, c.y - r), pos2(c.x - r, c.y + r)], stroke);
}

/// Window-control glyphs: – □ ✕ as crisp vector strokes.
fn paint_win_icon(ui: &Ui, c: Pos2, ctl: WinCtl, color: Color32) {
    let stroke = Stroke::new(1.1, color);
    let p = ui.painter();
    match ctl {
        WinCtl::Min => {
            let y = (c.y + 0.5).round() - 0.5;
            p.line_segment([pos2(c.x - WIN_ICON, y), pos2(c.x + WIN_ICON, y)], stroke);
        }
        WinCtl::Max => {
            let r = Rect::from_center_size(c, vec2(WIN_ICON * 2.0, WIN_ICON * 2.0));
            for pair in [
                [pos2(r.left(), r.top()), pos2(r.right(), r.top())],
                [pos2(r.right(), r.top()), pos2(r.right(), r.bottom())],
                [pos2(r.right(), r.bottom()), pos2(r.left(), r.bottom())],
                [pos2(r.left(), r.bottom()), pos2(r.left(), r.top())],
            ] {
                p.line_segment(pair, stroke);
            }
        }
        WinCtl::Close => paint_x(ui, c, WIN_ICON, color),
    }
}

// ---------------------------------------------------------------------------
// tests (pure logic only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::ellipsized_chars;

    #[test]
    fn no_ellipsis_when_it_fits() {
        assert_eq!(ellipsized_chars(&[5.0, 5.0, 5.0], 15.0, 4.0), 3);
        assert_eq!(ellipsized_chars(&[5.0, 5.0, 5.0], 50.0, 4.0), 3);
        assert_eq!(ellipsized_chars(&[], 10.0, 4.0), 0);
    }

    #[test]
    fn ellipsis_reserves_its_own_room() {
        // 15 > 14 → truncate, reserving the ellipsis: 5+4=9≤14, 10+4=14≤14 → keep 2
        assert_eq!(ellipsized_chars(&[5.0, 5.0, 5.0], 14.0, 4.0), 2);
        // total 15 fits in 19 → no ellipsis at all
        assert_eq!(ellipsized_chars(&[5.0, 5.0, 5.0], 19.0, 4.0), 3);
        // even one char cannot survive if char + ellipsis overflow
        assert_eq!(ellipsized_chars(&[5.0], 3.0, 4.0), 0);
    }

    #[test]
    fn ragged_widths_cut_at_the_last_fit() {
        // 2 + 4 + 1 + 8 = 15 > 12; 2 + 4 + 1 = 7, +3 ell = 10 ≤ 12, +8 > 12
        assert_eq!(ellipsized_chars(&[2.0, 4.0, 1.0, 8.0], 12.0, 3.0), 3);
    }
}
