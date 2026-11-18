//! The editor's colours: Roblox Studio's dark theme, sampled from a
//! screenshot of Studio (offline Studio) — panel #191a1f, headers #202227, menu bar
//! #272930, fields #2c2d33, active tab #33343a, dividers #353741, accent
//! #335fff (the Play button) — and Studio's "Select Color" (0.1, 0.6, 1) /
//! "Hover Over Color" (0.7, 0.9, 1) settings. rustudio's name and icons stay
//! its own.

use egui::{Color32, CornerRadius, Stroke};

pub const BG: Color32 = Color32::from_rgb(25, 26, 31);
pub const PANEL: Color32 = Color32::from_rgb(25, 26, 31);
pub const PANEL_ALT: Color32 = Color32::from_rgb(44, 45, 51);
pub const PANEL_DARK: Color32 = Color32::from_rgb(32, 34, 39);
/// The menu bar.
pub const MENU_BAR: Color32 = Color32::from_rgb(39, 41, 48);
/// The selected ribbon tab / toggled tool.
pub const ACTIVE_TAB: Color32 = Color32::from_rgb(51, 52, 58);
pub const DIVIDER: Color32 = Color32::from_rgb(53, 55, 65);
pub const TEXT: Color32 = Color32::from_rgb(222, 223, 226);
pub const TEXT_DIM: Color32 = Color32::from_rgb(125, 127, 132);
pub const ACCENT: Color32 = Color32::from_rgb(51, 95, 255);
pub const SELECT: Color32 = Color32::from_rgb(25, 153, 255);
pub const SELECT_BG: Color32 = Color32::from_rgb(38, 62, 120);
pub const HOVER_BG: Color32 = Color32::from_rgb(44, 45, 51);
/// 3D hover outline (Studio's "Hover Over Color").
pub const HOVER_3D: Color32 = Color32::from_rgb(179, 230, 255);
pub const WARN: Color32 = Color32::from_rgb(240, 200, 90);
pub const ERROR: Color32 = Color32::from_rgb(240, 96, 86);

pub fn apply(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = PANEL_DARK;
    v.faint_bg_color = Color32::from_rgb(30, 31, 37);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = SELECT_BG;
    v.selection.stroke = Stroke::new(1.0, SELECT);
    v.hyperlink_color = ACCENT;
    v.window_corner_radius = CornerRadius::same(7);
    v.menu_corner_radius = CornerRadius::same(7);
    let popup_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(110),
    };
    v.window_shadow = popup_shadow;
    v.popup_shadow = popup_shadow;
    v.window_stroke = Stroke::new(1.0, Color32::from_rgb(0x32, 0x35, 0x40));
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER);
    v.widgets.inactive.weak_bg_fill = PANEL_ALT;
    v.widgets.inactive.bg_fill = PANEL_ALT;
    v.widgets.hovered.weak_bg_fill = HOVER_BG;
    v.widgets.active.weak_bg_fill = SELECT_BG;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(5);
    }
    style.spacing.item_spacing = egui::vec2(6.0, 4.0);
    // Studio's menus and panels appear at once (no fades).
    style.animation_time = 0.0;
    style.spacing.button_padding = egui::vec2(6.0, 2.0);
    ctx.set_global_style(style);
}

pub fn dock_style(style: &egui::Style) -> egui_dock::Style {
    let mut s = egui_dock::Style::from_egui(style);
    let hairline = Color32::from_rgb(0x24, 0x26, 0x2e);
    let tab_radius = CornerRadius { nw: 5, ne: 5, sw: 0, se: 0 };
    s.tab_bar.bg_fill = Color32::from_rgb(0x13, 0x14, 0x18);
    s.tab_bar.hline_color = hairline;
    s.tab_bar.height = 24.0;
    // Studio's panels have a full-width title bar with the name centred.
    s.tab_bar.fill_tab_bar = true;
    s.tab.active.bg_fill = PANEL;
    s.tab.active.text_color = TEXT;
    s.tab.active.outline_color = ACCENT.gamma_multiply(0.75);
    s.tab.active.corner_radius = tab_radius;
    s.tab.focused.bg_fill = PANEL;
    s.tab.focused.text_color = Color32::WHITE;
    s.tab.focused.outline_color = ACCENT;
    s.tab.focused.corner_radius = tab_radius;
    s.tab.active_with_kb_focus = s.tab.focused.clone();
    s.tab.focused_with_kb_focus = s.tab.focused.clone();
    s.tab.inactive.bg_fill = Color32::from_rgb(0x17, 0x18, 0x1d);
    s.tab.inactive.text_color = TEXT_DIM;
    s.tab.inactive.outline_color = Color32::from_rgb(0x1f, 0x21, 0x28);
    s.tab.inactive.corner_radius = tab_radius;
    s.tab.hovered.bg_fill = Color32::from_rgb(0x22, 0x24, 0x2d);
    s.tab.hovered.text_color = TEXT;
    s.tab.hovered.outline_color = Color32::from_rgb(0x32, 0x35, 0x40);
    s.tab.hovered.corner_radius = tab_radius;
    s.tab.tab_body.bg_fill = PANEL;
    s.tab.tab_body.inner_margin = egui::Margin::same(4);
    s.tab.tab_body.stroke = Stroke::new(1.0, hairline);
    s.separator.width = 2.0;
    s.separator.color_idle = hairline;
    s.separator.color_hovered = ACCENT;
    s.separator.color_dragged = SELECT;
    s.overlay.selection_color = ACCENT.gamma_multiply(0.4);
    s
}
