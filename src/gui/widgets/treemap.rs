//! Squarified treemap of reclaimable space.
//!
//! Each duplicate group is one rectangle, sized by
//! `size_bytes * (file_count - 1)` — the bytes we'd reclaim by
//! collapsing the group. Color encodes file size class so a wall of
//! big-orange tiles means "go delete some movies".

use egui::{vec2, Color32, FontId, Rect, RichText, Sense, Stroke, Ui};

use crate::gui::state::UiState;
use crate::gui::theme;

pub fn show(ui: &mut Ui, state: &UiState) {
    ui.label(RichText::new("Reclaimable map").color(theme::TEXT_LO).strong());
    ui.add_space(4.0);

    if state.duplicates.is_empty() {
        ui.label(
            RichText::new("no duplicates yet")
                .color(theme::TEXT_LO)
                .italics(),
        );
        return;
    }

    // Sort groups by savings descending, keep top N to avoid drawing
    // thousands of one-pixel rectangles.
    let mut tiles: Vec<Tile> = state
        .duplicates
        .iter()
        .filter_map(|g| {
            let dup_count = g.files.len().saturating_sub(1) as u64;
            if dup_count == 0 {
                return None;
            }
            let savings = g.size.saturating_mul(dup_count);
            Some(Tile {
                savings,
                size_each: g.size,
                count: g.files.len(),
                label: g
                    .files
                    .first()
                    .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .flatten()
                    .unwrap_or_default(),
            })
        })
        .collect();
    tiles.sort_by(|a, b| b.savings.cmp(&a.savings));
    if tiles.len() > 256 {
        tiles.truncate(256);
    }

    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), ui.available_height().max(160.0)),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, theme::BG);

    let total: f64 = tiles.iter().map(|t| t.savings as f64).sum();
    if total <= 0.0 {
        return;
    }

    squarify(&tiles, total, rect, &painter);
}

struct Tile {
    savings: u64,
    size_each: u64,
    count: usize,
    label: String,
}

fn squarify(tiles: &[Tile], total: f64, rect: Rect, painter: &egui::Painter) {
    // Classic squarified treemap: walk tiles largest-first, accumulate a
    // row along the shorter side of the remaining rectangle until the
    // next tile would worsen the worst aspect ratio, then commit the
    // row and continue with the leftover rect.
    if tiles.is_empty() || rect.area() <= 0.0 {
        return;
    }

    let scale = rect.area() as f64 / total;
    let mut remaining = rect;
    let mut row: Vec<&Tile> = Vec::new();
    let mut row_area = 0.0f64;
    let mut idx = 0;

    while idx < tiles.len() {
        let tile = &tiles[idx];
        let area = tile.savings as f64 * scale;
        let side = remaining.width().min(remaining.height()) as f64;
        let new_worst = worst(&row, row_area, area, side);
        let old_worst = if row.is_empty() {
            f64::INFINITY
        } else {
            worst(&row, row_area, 0.0, side)
        };

        if row.is_empty() || new_worst <= old_worst {
            row.push(tile);
            row_area += area;
            idx += 1;
        } else {
            remaining = layout_row(&row, row_area, remaining, painter);
            row.clear();
            row_area = 0.0;
        }
    }
    if !row.is_empty() {
        layout_row(&row, row_area, remaining, painter);
    }
}

fn worst(row: &[&Tile], row_area: f64, extra_area: f64, side: f64) -> f64 {
    let total = row_area + extra_area;
    if total <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let s = side * side;
    let mut max_area = 0.0f64;
    let mut min_area = f64::INFINITY;
    for t in row {
        let a = t.savings as f64;
        if a > max_area {
            max_area = a;
        }
        if a < min_area {
            min_area = a;
        }
    }
    if extra_area > max_area {
        max_area = extra_area;
    }
    if extra_area > 0.0 && extra_area < min_area {
        min_area = extra_area;
    }
    let total_sq = total * total;
    f64::max(s * max_area / total_sq, total_sq / (s * min_area))
}

fn layout_row(row: &[&Tile], row_area: f64, rect: Rect, painter: &egui::Painter) -> Rect {
    if row.is_empty() || rect.area() <= 0.0 {
        return rect;
    }
    let along_x = rect.width() >= rect.height();
    let side = if along_x { rect.height() } else { rect.width() } as f64;
    let thickness = (row_area / side) as f32;

    let mut cursor = if along_x { rect.left() } else { rect.top() };
    for tile in row {
        let extent = (tile.savings as f64 / row_area) as f32 * if along_x { rect.height() } else { rect.width() };
        let tile_rect = if along_x {
            Rect::from_min_size(
                egui::pos2(rect.left(), cursor),
                vec2(thickness, extent),
            )
        } else {
            Rect::from_min_size(
                egui::pos2(cursor, rect.top()),
                vec2(extent, thickness),
            )
        };
        draw_tile(painter, tile_rect, tile);
        cursor += extent;
    }

    if along_x {
        Rect::from_min_max(
            egui::pos2(rect.left() + thickness, rect.top()),
            rect.max,
        )
    } else {
        Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + thickness),
            rect.max,
        )
    }
}

fn draw_tile(painter: &egui::Painter, rect: Rect, tile: &Tile) {
    let inset = rect.shrink(1.0);
    let color = color_for_size(tile.size_each);
    painter.rect_filled(inset, 2.0, color);
    painter.rect_stroke(inset, 2.0, Stroke::new(0.5, theme::BG));

    if inset.width() < 40.0 || inset.height() < 18.0 {
        return;
    }
    let savings = theme::humansize(tile.savings);
    painter.text(
        inset.left_top() + vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        savings,
        FontId::proportional(12.0),
        contrast(color),
    );
    if inset.height() >= 36.0 && !tile.label.is_empty() {
        painter.text(
            inset.left_top() + vec2(4.0, 18.0),
            egui::Align2::LEFT_TOP,
            format!("×{}  {}", tile.count, truncate(&tile.label, 24)),
            FontId::proportional(10.0),
            contrast(color).gamma_multiply(0.8),
        );
    }
}

fn color_for_size(bytes: u64) -> Color32 {
    // Log scale across size classes: tiny=blue, KB=teal, MB=yellow,
    // GB+ = orange/red. Fast, no allocation.
    let mb = (bytes as f64 / 1_048_576.0).max(0.0001);
    let log = mb.log10().clamp(-2.0, 4.0); // -2 .. +4
    let t = (log + 2.0) / 6.0; // 0..1
    blend3(theme::COOL, theme::WARN, theme::HOT, t as f32)
}

fn blend3(a: Color32, b: Color32, c: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        blend(a, b, t * 2.0)
    } else {
        blend(b, c, (t - 0.5) * 2.0)
    }
}

fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t) as u8;
    Color32::from_rgb(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
    )
}

fn contrast(c: Color32) -> Color32 {
    let lum = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    if lum > 140.0 {
        theme::PANEL_DEEP
    } else {
        theme::TEXT_HI
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
