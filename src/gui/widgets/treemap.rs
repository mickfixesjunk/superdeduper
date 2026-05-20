//! Squarified treemap of reclaimable space.
//!
//! Each duplicate group is one rectangle, sized by
//! `size_bytes * (file_count - 1)` — the bytes we'd reclaim by
//! collapsing the group. Color encodes file size class so a wall of
//! big-orange tiles means "go delete some movies".
//!
//! On hover, the tile underneath shows a tooltip with the group's
//! full membership — paths, sizes, savings — so the user knows
//! exactly what they're looking at.

use std::path::PathBuf;

use egui::{vec2, Color32, FontId, Rect, RichText, Sense, Stroke, Ui};

use crate::gui::state::UiState;
use crate::gui::theme;

pub fn show(ui: &mut Ui, state: &UiState) {
    show_filtered(ui, state, None, &[])
}

pub fn show_filtered(
    ui: &mut Ui,
    state: &UiState,
    drive_root: Option<&std::path::Path>,
    reference_roots: &[std::path::PathBuf],
) {
    ui.label(
        RichText::new("Reclaimable map")
            .color(theme::TEXT_LO)
            .strong(),
    );
    ui.add_space(4.0);

    if state.duplicates.is_empty() {
        ui.label(
            RichText::new("No duplicates yet.")
                .color(theme::TEXT_LO)
                .italics(),
        );
        return;
    }

    let mut tiles: Vec<Tile> = state
        .duplicates
        .iter()
        .enumerate()
        .filter(|(_, g)| group_passes_filter(g, drive_root, reference_roots))
        .filter_map(|(idx, g)| {
            let dup_count = g.files.len().saturating_sub(1) as u64;
            if dup_count == 0 {
                return None;
            }
            let savings = g.size.saturating_mul(dup_count);
            Some(Tile {
                group_index: idx + 1,
                savings,
                size_each: g.size,
                count: g.files.len(),
                files: g.files.clone(),
                hash: g.content_hash.clone(),
            })
        })
        .collect();
    if tiles.is_empty() {
        ui.label(
            RichText::new("No duplicates on this drive.")
                .color(theme::TEXT_LO)
                .italics(),
        );
        return;
    }
    tiles.sort_by_key(|t| std::cmp::Reverse(t.savings));
    if tiles.len() > 256 {
        tiles.truncate(256);
    }
    let total: f64 = tiles.iter().map(|t| t.savings as f64).sum();
    if total <= 0.0 {
        return;
    }

    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), ui.available_height().max(160.0)),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, theme::BG);

    let placed = squarify(&tiles, total, rect);
    for placement in &placed {
        draw_tile(&painter, placement);
    }

    // Hover: find the tile under the cursor and show a tooltip.
    if let Some(pos) = response.hover_pos() {
        if let Some(placement) = placed.iter().find(|p| p.rect.contains(pos)) {
            response.clone().on_hover_ui_at_pointer(|ui| {
                tooltip(ui, placement.tile);
            });
        }
    }
}

struct Tile {
    group_index: usize,
    savings: u64,
    size_each: u64,
    count: usize,
    files: Vec<PathBuf>,
    hash: String,
}

struct Placement<'a> {
    tile: &'a Tile,
    rect: Rect,
}

fn group_passes_filter(
    g: &crate::gui::events::DuplicateGroupSummary,
    drive_root: Option<&std::path::Path>,
    reference_roots: &[std::path::PathBuf],
) -> bool {
    let Some(root) = drive_root else { return true };
    g.files.iter().any(|p| {
        p.starts_with(root) || reference_roots.iter().any(|r| p.starts_with(r))
    })
}

fn tooltip(ui: &mut egui::Ui, tile: &Tile) {
    ui.label(
        RichText::new(format!("Group #{}", tile.group_index))
            .strong()
            .color(theme::TEXT_HI),
    );
    ui.label(
        RichText::new(format!(
            "{} × {} copies   ·   {} reclaimable",
            theme::humansize(tile.size_each),
            tile.count,
            theme::humansize(tile.savings),
        ))
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.label(
        RichText::new(format!("hash {}…", &tile.hash[..12.min(tile.hash.len())]))
            .color(theme::TEXT_LO)
            .monospace()
            .small(),
    );
    ui.separator();
    for (i, p) in tile.files.iter().take(8).enumerate() {
        let tag = if i == 0 { "keep " } else { "dupe " };
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(tag)
                    .color(if i == 0 { theme::ACCENT } else { theme::TEXT_LO })
                    .monospace()
                    .small()
                    .strong(),
            );
            ui.label(
                RichText::new(p.to_string_lossy())
                    .color(theme::TEXT_HI)
                    .monospace()
                    .small(),
            );
        });
    }
    if tile.files.len() > 8 {
        ui.label(
            RichText::new(format!("… {} more", tile.files.len() - 8))
                .color(theme::TEXT_LO)
                .small(),
        );
    }
}

/// Classic squarified treemap. Walk tiles largest-first, accumulate a
/// row along the shorter side of the remaining rectangle until the
/// next tile would worsen the worst aspect ratio, then commit the
/// row and continue with the leftover rect.
fn squarify<'a>(tiles: &'a [Tile], total: f64, rect: Rect) -> Vec<Placement<'a>> {
    let mut out = Vec::with_capacity(tiles.len());
    if tiles.is_empty() || rect.area() <= 0.0 {
        return out;
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
        let try_add = worst(&row, row_area, Some(area), side);
        let without = if row.is_empty() {
            f64::INFINITY
        } else {
            worst(&row, row_area, None, side)
        };

        if row.is_empty() || try_add <= without {
            row.push(tile);
            row_area += area;
            idx += 1;
        } else {
            remaining = layout_row(&row, row_area, remaining, &mut out);
            row.clear();
            row_area = 0.0;
        }
    }
    if !row.is_empty() {
        layout_row(&row, row_area, remaining, &mut out);
    }
    out
}

fn worst(row: &[&Tile], row_area: f64, extra_area: Option<f64>, side: f64) -> f64 {
    let total = row_area + extra_area.unwrap_or(0.0);
    if total <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
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
    if let Some(ea) = extra_area {
        if ea > max_area {
            max_area = ea;
        }
        if ea < min_area {
            min_area = ea;
        }
    }
    let s = side * side;
    let total_sq = total * total;
    f64::max(s * max_area / total_sq, total_sq / (s * min_area))
}

/// Place the row along the SHORT side of the remaining rect. Returns
/// the leftover rect for the next row.
///
/// `row_area` is the row's total area in pixel-units (already scaled
/// from savings via `scale`). We also recover the unscaled
/// `row_savings` so the per-tile extent within the row is a pure
/// ratio with no scaling factor cancellation bugs.
///
/// If the rect is wider than tall (`along_x = true`), the row is a
/// **vertical strip on the left**: thickness in X, tiles stack along
/// Y. If the rect is taller than wide, the row is a **horizontal
/// strip on the top**: thickness in Y, tiles stack along X.
fn layout_row<'a>(
    row: &[&'a Tile],
    row_area: f64,
    rect: Rect,
    out: &mut Vec<Placement<'a>>,
) -> Rect {
    if row.is_empty() || rect.area() <= 0.0 {
        return rect;
    }
    let along_x = rect.width() >= rect.height();
    // The row spans the full SHORT side of the rect. Its thickness
    // (the LONG axis extent) is row_area / span.
    let span = if along_x { rect.height() } else { rect.width() } as f64;
    let thickness = (row_area / span) as f32;
    let row_savings: f64 = row.iter().map(|t| t.savings as f64).sum();

    // cursor steps along the SHORT side of the rect — Y when the row
    // is a vertical strip (along_x), X when it's horizontal.
    let mut cursor = if along_x { rect.top() } else { rect.left() };
    for tile in row {
        let extent = if row_savings > 0.0 {
            (tile.savings as f64 / row_savings) as f32 * span as f32
        } else {
            0.0
        };
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
        out.push(Placement {
            tile,
            rect: tile_rect,
        });
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

fn draw_tile(painter: &egui::Painter, placement: &Placement<'_>) {
    let inset = placement.rect.shrink(1.0);
    let color = color_for_size(placement.tile.size_each);
    painter.rect_filled(inset, 2.0, color);
    painter.rect_stroke(inset, 2.0, Stroke::new(0.5, theme::BG));

    if inset.width() < 40.0 || inset.height() < 18.0 {
        return;
    }
    let savings = theme::humansize(placement.tile.savings);
    painter.text(
        inset.left_top() + vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        savings,
        FontId::proportional(12.0),
        contrast(color),
    );
    if inset.height() >= 36.0 {
        let label = placement
            .tile
            .files
            .first()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        if !label.is_empty() {
            painter.text(
                inset.left_top() + vec2(4.0, 18.0),
                egui::Align2::LEFT_TOP,
                format!("×{}  {}", placement.tile.count, truncate(&label, 24)),
                FontId::proportional(10.0),
                contrast(color).gamma_multiply(0.8),
            );
        }
    }
}

fn color_for_size(bytes: u64) -> Color32 {
    let mb = (bytes as f64 / 1_048_576.0).max(0.0001);
    let log = mb.log10().clamp(-2.0, 4.0);
    let t = (log + 2.0) / 6.0;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tile(group: usize, savings: u64, count: usize) -> Tile {
        Tile {
            group_index: group,
            savings,
            size_each: if count > 0 { savings / count.max(1) as u64 } else { savings },
            count,
            files: (0..count).map(|i| PathBuf::from(format!("/x/g{group}/{i}"))).collect(),
            hash: "deadbeef".repeat(8),
        }
    }

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(w, h))
    }

    /// Total tile area covers the parent rect (up to a small float
    /// rounding margin). Catches algorithm-level bugs where tiles
    /// don't fill the canvas.
    #[test]
    fn tiles_cover_the_rect() {
        let tiles = vec![
            tile(1, 600, 2),
            tile(2, 300, 2),
            tile(3, 100, 2),
        ];
        let total: f64 = tiles.iter().map(|t| t.savings as f64).sum();
        let canvas = rect(400.0, 300.0);
        let placed = squarify(&tiles, total, canvas);
        let placed_area: f32 = placed.iter().map(|p| p.rect.area()).sum();
        let canvas_area = canvas.area();
        let diff = (placed_area - canvas_area).abs() / canvas_area;
        assert!(
            diff < 0.005,
            "tiles cover {} of canvas {}: diff {}",
            placed_area,
            canvas_area,
            diff,
        );
    }

    /// No two tiles overlap, and every tile sits inside the canvas.
    #[test]
    fn tiles_dont_overlap_or_escape() {
        let tiles = vec![
            tile(1, 1000, 2),
            tile(2, 700, 2),
            tile(3, 400, 2),
            tile(4, 200, 2),
            tile(5, 50, 2),
        ];
        let total: f64 = tiles.iter().map(|t| t.savings as f64).sum();
        let canvas = rect(500.0, 200.0);
        let placed = squarify(&tiles, total, canvas);
        for (i, a) in placed.iter().enumerate() {
            // contains_rect is exclusive on the upper boundary; use a
            // tolerant check so float drift doesn't trip the assertion.
            let tol = 0.01;
            assert!(
                a.rect.min.x >= canvas.min.x - tol
                    && a.rect.min.y >= canvas.min.y - tol
                    && a.rect.max.x <= canvas.max.x + tol
                    && a.rect.max.y <= canvas.max.y + tol,
                "tile {} escapes canvas: {:?} vs {:?}",
                i,
                a.rect,
                canvas,
            );
            for b in placed.iter().skip(i + 1) {
                let overlap = overlap_area(a.rect, b.rect);
                assert!(
                    overlap < 1.0,
                    "tiles overlap (area={overlap}): {:?} ∩ {:?}",
                    a.rect,
                    b.rect
                );
            }
        }
    }

    /// True overlap area, clamped to zero for disjoint rects so the
    /// negative-side-length quirk of egui's Rect::intersect can't fool
    /// us into reporting a false overlap.
    fn overlap_area(a: Rect, b: Rect) -> f32 {
        let dx = (a.max.x.min(b.max.x) - a.min.x.max(b.min.x)).max(0.0);
        let dy = (a.max.y.min(b.max.y) - a.min.y.max(b.min.y)).max(0.0);
        dx * dy
    }

    /// Tile area is proportional to savings, within a few percent.
    #[test]
    fn tile_area_proportional_to_savings() {
        let tiles = vec![tile(1, 800, 2), tile(2, 200, 2)];
        let total: f64 = tiles.iter().map(|t| t.savings as f64).sum();
        let canvas = rect(400.0, 400.0);
        let placed = squarify(&tiles, total, canvas);
        assert_eq!(placed.len(), 2);
        let a_area = placed[0].rect.area() as f64;
        let b_area = placed[1].rect.area() as f64;
        let total_area = a_area + b_area;
        let a_share = a_area / total_area;
        let b_share = b_area / total_area;
        assert!(
            (a_share - 0.8).abs() < 0.02,
            "a expected ~0.80, got {}",
            a_share
        );
        assert!(
            (b_share - 0.2).abs() < 0.02,
            "b expected ~0.20, got {}",
            b_share
        );
    }
}
