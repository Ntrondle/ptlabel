//! Block-type icons, painted as vectors so they stay crisp at any size and follow
//! the theme's text colour (no icon font needed).

use egui::{Color32, Painter, Pos2, Rect, Response, Shape, Stroke, Ui, Vec2, pos2, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Text,
    Qr,
    DataMatrix,
    Barcode,
    Image,
    Spacer,
    Printer,
    Sequence,
    Sheet,
    ChevronDown,
    ChevronRight,
    AlignLeft,
    AlignCenter,
    AlignRight,
    Scissors,
}

impl Icon {
    /// Width / height (the "123" icon is wider so the digits stay legible).
    pub fn aspect(self) -> f32 {
        if self == Icon::Sequence { 1.5 } else { 1.0 }
    }
}

/// Paint `icon` into the square `r` in `col`.
pub fn paint(p: &Painter, r: Rect, icon: Icon, col: Color32) {
    let asp = icon.aspect();
    let side = r.height().min(r.width() / asp);
    let r = Rect::from_center_size(r.center(), vec2(side * asp, side));
    let at = |x: f32, y: f32| -> Pos2 { pos2(r.min.x + x * side, r.min.y + y * side) };
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(at(x0, y0), at(x1, y1));
    let fill = |x0, y0, x1, y1| p.rect_filled(rect(x0, y0, x1, y1), 0.0, col);
    let sw = (side / 10.0).max(1.4);
    let stroke = Stroke::new(sw, col);
    let tri = |a: Pos2, b: Pos2, c: Pos2| p.add(Shape::convex_polygon(vec![a, b, c], col, Stroke::NONE));

    match icon {
        Icon::Text => {
            fill(0.14, 0.10, 0.86, 0.27);
            fill(0.41, 0.10, 0.59, 0.90);
            fill(0.30, 0.80, 0.70, 0.90);
        }
        Icon::Qr => {
            let h = sw / side / 2.0;
            for (x, y) in [(0.0, 0.0), (0.58, 0.0), (0.0, 0.58)] {
                p.rect_stroke(rect(x + h, y + h, x + 0.42 - h, y + 0.42 - h), 0.0, stroke, egui::StrokeKind::Middle);
                fill(x + 0.13, y + 0.13, x + 0.29, y + 0.29);
            }
            fill(0.58, 0.58, 0.76, 0.76);
            fill(0.82, 0.82, 1.0, 1.0);
            fill(0.82, 0.58, 1.0, 0.70);
            fill(0.58, 0.88, 0.70, 1.0);
        }
        Icon::DataMatrix => {
            // 6x6 modules: solid L finder (left + bottom), timing on top + right.
            let dark: [&str; 6] = ["#.#.#.", "#.##.#", "##..#.", "#..#.#", "##.##.", "######"];
            let u = 1.0 / 6.0;
            for (y, row) in dark.iter().enumerate() {
                for (x, c) in row.chars().enumerate() {
                    if c == '#' {
                        let (x, y) = (x as f32 * u, y as f32 * u);
                        fill(x, y, x + u, y + u);
                    }
                }
            }
        }
        Icon::Barcode => {
            // (start, width) in 1/15 units.
            for (x, w) in [(0.0, 2.0), (3.0, 1.0), (5.0, 2.0), (8.0, 1.0), (10.0, 1.0), (12.0, 3.0)] {
                fill(x / 15.0, 0.10, (x + w) / 15.0, 0.90);
            }
        }
        Icon::Image => {
            p.rect_stroke(rect(0.04, 0.12, 0.96, 0.88), side * 0.08, stroke, egui::StrokeKind::Middle);
            tri(at(0.16, 0.76), at(0.42, 0.42), at(0.66, 0.76));
            tri(at(0.50, 0.76), at(0.68, 0.54), at(0.86, 0.76));
            p.circle_filled(at(0.72, 0.33), side * 0.08, col);
        }
        Icon::Printer => {
            let line = |pts: &[(f32, f32)]| {
                p.add(Shape::line(pts.iter().map(|&(x, y)| at(x, y)).collect(), stroke));
            };
            line(&[(0.27, 0.31), (0.27, 0.07), (0.73, 0.07), (0.73, 0.31)]); // paper in
            line(&[(0.27, 0.74), (0.06, 0.74), (0.06, 0.31), (0.94, 0.31), (0.94, 0.74), (0.73, 0.74)]); // body
            p.rect_stroke(rect(0.27, 0.56, 0.73, 0.93), 0.0, stroke, egui::StrokeKind::Middle); // paper out
            let thin = Stroke::new(sw * 0.7, col);
            p.line_segment([at(0.37, 0.70), at(0.63, 0.70)], thin);
            p.line_segment([at(0.37, 0.81), at(0.55, 0.81)], thin);
            p.circle_filled(at(0.81, 0.44), side * 0.05, col);
        }
        Icon::Sequence => {
            p.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                "123",
                egui::FontId::new(side * 0.85, egui::FontFamily::Name("bold".into())),
                col,
            );
        }
        Icon::Sheet => {
            p.rect_stroke(rect(0.04, 0.10, 0.96, 0.90), side * 0.08, stroke, egui::StrokeKind::Middle);
            p.rect_filled(rect(0.04, 0.10, 0.96, 0.36), egui::CornerRadius { nw: (side * 0.08) as u8, ne: (side * 0.08) as u8, sw: 0, se: 0 }, col);
            p.line_segment([at(0.38, 0.36), at(0.38, 0.90)], stroke);
            p.line_segment([at(0.04, 0.63), at(0.96, 0.63)], stroke);
        }
        Icon::Spacer => {
            p.line_segment([at(0.06, 0.18), at(0.06, 0.82)], stroke);
            p.line_segment([at(0.94, 0.18), at(0.94, 0.82)], stroke);
            p.line_segment([at(0.30, 0.50), at(0.70, 0.50)], stroke);
            tri(at(0.17, 0.50), at(0.38, 0.33), at(0.38, 0.67));
            tri(at(0.83, 0.50), at(0.62, 0.33), at(0.62, 0.67));
        }
        Icon::ChevronDown => {
            let bold = Stroke::new((side / 7.0).max(1.8), col);
            p.add(Shape::line(vec![at(0.18, 0.32), at(0.50, 0.68), at(0.82, 0.32)], bold));
        }
        Icon::ChevronRight => {
            let bold = Stroke::new((side / 7.0).max(1.8), col);
            p.add(Shape::line(vec![at(0.32, 0.18), at(0.68, 0.50), at(0.32, 0.82)], bold));
        }
        Icon::AlignLeft | Icon::AlignCenter | Icon::AlignRight => {
            let h = 0.115;
            let rad = side * 0.04;
            let bars: [(f32, f32); 4] = [(0.14, 0.86), (0.37, 0.56), (0.60, 0.76), (0.83, 0.44)];
            for (cy, w) in bars {
                let x0 = match icon {
                    Icon::AlignLeft => 0.07,
                    Icon::AlignCenter => 0.5 - w * 0.5,
                    Icon::AlignRight => 0.93 - w,
                    _ => unreachable!(),
                };
                p.rect_filled(rect(x0, cy - h * 0.5, x0 + w, cy + h * 0.5), rad, col);
            }
        }
        Icon::Scissors => {
            let blade = Stroke::new((side / 9.5).max(1.8), col);
            let ring = Stroke::new((side / 10.0).max(1.6), col);
            // Finger rings on the left
            p.circle_stroke(at(0.24, 0.27), side * 0.15, ring);
            p.circle_stroke(at(0.24, 0.73), side * 0.15, ring);
            // Crossed blades extending to the right
            p.line_segment([at(0.36, 0.34), at(0.90, 0.76)], blade);
            p.line_segment([at(0.36, 0.66), at(0.90, 0.24)], blade);
            // Center pivot screw
            p.circle_filled(at(0.55, 0.50), side * 0.055, col);
        }
    }
}

/// The PTLabel mark: tape running across a blue tile (yellow in dev builds) with a cut on each side
/// (same geometry as assets/icon.png, see scripts/make_icon.py).
pub fn paint_logo(p: &Painter, r: Rect) {
    let side = r.width().min(r.height());
    let r = Rect::from_center_size(r.center(), Vec2::splat(side));
    let at = |x: f32, y: f32| -> Pos2 { pos2(r.min.x + x * side, r.min.y + y * side) };
    #[cfg(feature = "dev")]
    let tile = Color32::from_rgb(238, 178, 24);
    #[cfg(not(feature = "dev"))]
    let tile = Color32::from_rgb(58, 123, 234);
    p.rect_filled(r, side * 0.225, tile);
    p.rect_filled(Rect::from_min_max(at(0.0, 0.35), at(1.0, 0.65)), 0.0, Color32::WHITE);
    p.rect_filled(Rect::from_min_max(at(0.34, 0.455), at(0.66, 0.545)), side * 0.045, Color32::from_rgb(28, 37, 51));
    for x in [0.22, 0.78] {
        for (y0, y1) in [(0.35, 0.405), (0.65, 0.595)] {
            let pts = vec![at(x - 0.05, y0), at(x + 0.05, y0), at(x, y1)];
            p.add(Shape::convex_polygon(pts, tile, Stroke::NONE));
        }
        p.line_segment([at(x, 0.40), at(x, 0.60)], Stroke::new((side * 0.03).max(1.0), tile));
    }
}

pub fn logo(ui: &mut Ui, size: f32) -> Response {
    let (r, resp) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    paint_logo(ui.painter(), r);
    resp
}

/// Standalone icon (e.g. in a header).
pub fn show(ui: &mut Ui, icon: Icon, size: f32) -> Response {
    let (r, resp) = ui.allocate_exact_size(vec2(size * icon.aspect(), size), egui::Sense::hover());
    let col = ui.visuals().strong_text_color();
    paint(ui.painter(), r, icon, col);
    resp
}

/// Upside-down arrow button to minimize/expand a panel or block box.
pub fn collapse_button(ui: &mut Ui, collapsed: &mut bool) -> Response {
    let icon = if *collapsed { Icon::ChevronRight } else { Icon::ChevronDown };
    let (rect, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&resp);
        if resp.hovered() || resp.is_pointer_button_down_on() {
            ui.painter().rect_filled(rect, 4.0, visuals.weak_bg_fill);
        }
        paint(ui.painter(), rect.shrink(5.0), icon, visuals.text_color());
    }
    if resp.clicked() {
        *collapsed = !*collapsed;
    }
    resp.on_hover_text(if *collapsed { "Expand" } else { "Minimize" })
}

/// Native button with a leading icon.
pub fn button(ui: &mut Ui, icon: Icon, text: &str) -> Response {
    let id = egui::IdSalt::new(icon);
    let out = egui::Button::new((egui::Atom::custom(id, vec2(18.0 * icon.aspect(), 18.0)), egui::RichText::new(text).size(14.0)))
        .min_size(vec2(0.0, 32.0))
        .atom_ui(ui);
    if let Some(r) = out.rect(id) {
        let col = ui.style().interact(&out.response).text_color();
        paint(ui.painter(), r, icon, col);
    }
    out.response
}

/// Compact square toggle button with a centred vector icon (used for text alignment bars).
pub fn toggle_icon(ui: &mut Ui, selected: bool, icon: Icon, tooltip: &str) -> Response {
    let id = egui::IdSalt::new(icon);
    let h = ui.spacing().interact_size.y.max(28.0);
    let out = egui::Button::new(egui::Atom::custom(id, vec2(15.0 * icon.aspect(), 15.0)))
        .selected(selected)
        .min_size(vec2(30.0, h))
        .atom_ui(ui);
    if let Some(r) = out.rect(id) {
        let col = ui.style().interact_selectable(&out.response, selected).text_color();
        paint(ui.painter(), r, icon, col);
    }
    out.response.on_hover_text(tooltip)
}
