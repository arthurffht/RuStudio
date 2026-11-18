//! Tabler Icons (MIT, https://tabler.io/icons; app/assets/tabler, credited
//! in CREDITS.md): the outline and filled icon fonts, drawn by name. Every
//! ribbon, menu and Explorer icon comes from this one set so they read as
//! one family; colours are rustudio's own.
//!
//! The editor's text font is Studio's Builder Sans, read at run time from
//! the content folder like the other Studio files (CREDITS.md), else egui's.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use egui::{Align2, Color32, FontFamily, FontId, Painter, Rect};

fn tabler_family() -> FontFamily {
    static F: OnceLock<FontFamily> = OnceLock::new();
    F.get_or_init(|| FontFamily::Name(Arc::from("tabler"))).clone()
}

fn tabler_filled_family() -> FontFamily {
    static F: OnceLock<FontFamily> = OnceLock::new();
    F.get_or_init(|| FontFamily::Name(Arc::from("tabler-filled"))).clone()
}

const OUTLINE: &[u8] = include_bytes!("../assets/tabler/tabler-icons.ttf");
const FILLED: &[u8] = include_bytes!("../assets/tabler/tabler-icons-filled.ttf");

fn table(text: &'static str) -> HashMap<&'static str, char> {
    text.lines()
        .filter_map(|l| {
            let (n, c) = l.split_once(' ')?;
            Some((n, char::from_u32(u32::from_str_radix(c, 16).ok()?)?))
        })
        .collect()
}

fn outline() -> &'static HashMap<&'static str, char> {
    static T: OnceLock<HashMap<&'static str, char>> = OnceLock::new();
    T.get_or_init(|| table(include_str!("../assets/tabler/codepoints.txt")))
}

fn filled() -> &'static HashMap<&'static str, char> {
    static T: OnceLock<HashMap<&'static str, char>> = OnceLock::new();
    T.get_or_init(|| table(include_str!("../assets/tabler/codepoints-filled.txt")))
}

/// rustudio's own drawn icons (not glyphs). `rs-model`: three coloured
/// shapes in a triangle (pink triangle on top, blue circle bottom left,
/// amber square bottom right), Roblox's colourful Model idea with
/// rustudio's own shapes.
const DRAWN: &[&str] = &["rs-model"];

fn paint_model(p: &Painter, r: Rect) {
    use egui::{pos2, Pos2, Shape, Stroke};
    let n = r.width().min(r.height());
    let o = r.center() - egui::vec2(n, n) / 2.0;
    let pad = n * 0.06;
    let c = (n - 2.0 * pad) / 2.0;
    let g = n * 0.10;
    let h = c - g / 2.0;
    let (x0, y0) = (o.x + pad, o.y + pad);
    let (x1, y1) = (x0 + c + g / 2.0, y0 + c + g / 2.0);
    let cx = o.x + n / 2.0;
    let rgb = Color32::from_rgb;
    let tri: Vec<Pos2> = vec![pos2(cx, y0), pos2(cx + h / 2.0, y0 + h), pos2(cx - h / 2.0, y0 + h)];
    p.add(Shape::convex_polygon(tri, rgb(255, 95, 150), Stroke::NONE));
    p.circle_filled(pos2(x0 + h / 2.0, y1 + h / 2.0), h / 2.0, rgb(80, 170, 255));
    p.rect_filled(
        Rect::from_min_size(pos2(x1, y1), egui::vec2(h, h)),
        egui::CornerRadius::same((h * 0.22).round() as u8),
        rgb(255, 184, 60),
    );
}

/// Whether the set has an icon of that name (`-filled` suffix: the filled one).
pub fn has(name: &str) -> bool {
    if DRAWN.contains(&name) {
        return true;
    }
    match name.strip_suffix("-filled") {
        Some(n) => filled().contains_key(n),
        None => outline().contains_key(name),
    }
}

/// The glyph and font family for an icon name ("cube", "player-play-filled").
pub fn glyph(name: &str) -> Option<(char, FontFamily)> {
    match name.strip_suffix("-filled") {
        Some(n) => filled().get(n).map(|c| (*c, tabler_filled_family())),
        None => outline().get(name).map(|c| (*c, tabler_family())),
    }
}

/// Paints icon `name` centred in `r`, sized to fit.
pub fn paint(p: &Painter, r: Rect, name: &str, color: Color32) {
    if name == "rs-model" {
        // its own colours, whatever the caller's tint
        let _ = color;
        paint_model(p, r);
        return;
    }
    let name = if has(name) { name } else { "square" };
    let Some((c, family)) = glyph(name) else {
        return;
    };
    let size = r.width().min(r.height());
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    p.text(
        r.center(),
        Align2::CENTER_CENTER,
        s,
        FontId::new(size, family),
        color,
    );
}

/// Adds the icon fonts and Studio's Builder Sans / Windows fallback fonts to egui.
pub fn install(ctx: &egui::Context) {
    let mut defs = egui::FontDefinitions::default();
    defs.font_data.insert(
        "tabler".into(),
        Arc::new(egui::FontData::from_static(OUTLINE)),
    );
    defs.font_data.insert(
        "tabler-filled".into(),
        Arc::new(egui::FontData::from_static(FILLED)),
    );
    defs.families
        .insert(FontFamily::Name("tabler".into()), vec!["tabler".into()]);
    defs.families.insert(
        FontFamily::Name("tabler-filled".into()),
        vec!["tabler-filled".into()],
    );
    let fonts = crate::assets::content_dir().join("fonts");
    let win_fonts = std::path::Path::new(r"C:\Windows\Fonts");
    // Promote Windows Segoe UI ahead of egui's Ubuntu-Light when BuilderSans isn't installed.
    for (key, path, family) in [
        ("segoe-ui", win_fonts.join("segoeui.ttf"), FontFamily::Proportional),
        ("segoe-ui-bold", win_fonts.join("segoeuib.ttf"), FontFamily::Name("bold".into())),
        ("segoe-ui-semibold", win_fonts.join("seguisb.ttf"), FontFamily::Name("medium".into())),
    ] {
        if let Ok(bytes) = std::fs::read(&path) {
            defs.font_data.insert(key.into(), Arc::new(egui::FontData::from_owned(bytes)));
            defs.families.entry(family).or_default().insert(0, key.into());
        }
    }
    for (key, file, family) in [
        ("builder-sans", "BuilderSans-Regular.otf", FontFamily::Proportional),
        ("builder-sans-bold", "BuilderSans-Bold.otf", FontFamily::Name("bold".into())),
        // (the in-game menu's buttons and tabs are Builder Sans Medium)
        ("builder-sans-medium", "BuilderSans-Medium.otf", FontFamily::Name("medium".into())),
    ] {
        if let Ok(bytes) = std::fs::read(fonts.join(file)) {
            defs.font_data.insert(
                key.into(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            let list = defs.families.entry(family).or_default();
            list.insert(0, key.into());
        }
    }
    // Studio's Output is monospace, warnings and errors in bold: BuilderMono,
    // then FiraMono / RobotoMono / CascadiaMono / Consolas ahead of Hack.
    let local_cascadia = std::env::var_os("LOCALAPPDATA")
        .map(|d| std::path::PathBuf::from(d).join(r"Microsoft\Windows\Fonts\CascadiaMono.ttf"));
    let mono_regular: &[(&str, std::path::PathBuf)] = &[
        ("builder-mono", fonts.join("BuilderMono-Regular.otf")),
        ("fira-mono", fonts.join("FiraMono-Regular.ttf")),
        ("roboto-mono", fonts.join("RobotoMono-Regular.ttf")),
        ("cascadia-mono", win_fonts.join("CascadiaMono.ttf")),
        ("consolas", win_fonts.join("consola.ttf")),
    ];
    let mut picked_mono = None;
    for (key, path) in mono_regular {
        let bytes = std::fs::read(path).ok().or_else(|| {
            (*key == "cascadia-mono")
                .then(|| local_cascadia.as_ref().and_then(|p| std::fs::read(p).ok()))
                .flatten()
        });
        if let Some(bytes) = bytes {
            defs.font_data.insert((*key).into(), Arc::new(egui::FontData::from_owned(bytes)));
            defs.families.entry(FontFamily::Monospace).or_default().insert(0, (*key).into());
            picked_mono = Some(*key);
            break;
        }
    }
    let mono_bold: &[(&str, std::path::PathBuf)] = &[
        ("builder-mono-bold", fonts.join("BuilderMono-Bold.otf")),
        ("fira-mono-bold", fonts.join("FiraMono-Bold.ttf")),
        ("roboto-mono-bold", fonts.join("RobotoMono-Bold.ttf")),
        ("consolas-bold", win_fonts.join("consolab.ttf")),
    ];
    for (key, path) in mono_bold {
        if let Ok(bytes) = std::fs::read(path) {
            defs.font_data.insert((*key).into(), Arc::new(egui::FontData::from_owned(bytes)));
            defs.families.entry(FontFamily::Name("mono-bold".into())).or_default().insert(0, (*key).into());
            break;
        }
    }
    let _ = picked_mono;
    let mono = defs.families[&FontFamily::Monospace].clone();
    defs.families.entry(FontFamily::Name("mono-bold".into())).or_default().extend(mono);
    // Symbols the faces lack (a game's text in egui's default font, "✓"):
    // the same fallback fonts as game fonts get.
    for (i, bytes) in crate::gui::fallback_fonts().iter().enumerate() {
        let key = format!("fallback{i}");
        defs.font_data.insert(key.clone(), Arc::new(egui::FontData::from_static(bytes)));
        defs.families.entry(FontFamily::Proportional).or_default().push(key);
    }
    // Bold falls back to the regular face (and then egui's) when missing.
    let prop = defs.families[&FontFamily::Proportional].clone();
    defs.families
        .entry(FontFamily::Name("bold".into()))
        .or_default()
        .extend(prop.clone());
    defs.families
        .entry(FontFamily::Name("medium".into()))
        .or_default()
        .extend(prop);
    ctx.set_fonts(defs);
}
