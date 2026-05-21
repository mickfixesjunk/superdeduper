//! Per-drive live scope. For each detected physical drive we render:
//!
//! * a header row with model, type (HDD/SSD), and current MB/s,
//! * a throughput sparkline (last 30 s of bytes-per-second),
//! * a 2-D LCN-vs-time read trace — the killer visualization that
//!   shows the LCN-sorted read scheduler doing its job. On an HDD the
//!   dots crawl monotonically up the Y axis; on an SSD they spray.

use std::time::Instant;

use egui::{vec2, Color32, FontId, RichText, Sense, Stroke, Ui};

use crate::gui::state::{DriveLive, UiState, THROUGHPUT_WINDOW_SECS};
use crate::gui::theme;

const SCOPE_HEIGHT: f32 = 120.0;
const SPARK_HEIGHT: f32 = 36.0;

/// Returns the drive id the user clicked this frame (caller toggles a
/// selection in App state and the Groups / Treemap filter to that
/// drive's paths). `render_overrides[id] = Some(true)` forces SSD-
/// style render even if the device was detected as an HDD; `None` =
/// use auto-detected `has_seek_penalty`. The widget mutates the map
/// when the user clicks the "HDD"/"SSD" badge in a panel.
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    selected: Option<u32>,
    render_overrides: &mut hashbrown::HashMap<u32, bool>,
    // When the scan is finished, pass Some(scan-end-instant) so the
    // age math freezes instead of decaying after reads stop arriving.
    frozen_now: Option<Instant>,
) -> Option<u32> {
    let mut clicked: Option<u32> = None;
    ui.label(RichText::new("Drive scope").color(theme::TEXT_LO).strong())
        .on_hover_text(
            "Per-drive read activity. Click a drive panel to filter the \
             Groups and Treemap below to duplicates that live on that \
             drive — click again or pick another drive to clear. Click \
             the HDD / SSD badge to override the auto-detected render \
             style.",
        );
    ui.add_space(4.0);

    if state.drives.is_empty() {
        ui.label(
            RichText::new("waiting for drives…")
                .color(theme::TEXT_LO)
                .italics(),
        );
        return clicked;
    }

    let mut ids: Vec<_> = state.drives.keys().copied().collect();
    ids.sort();

    let now = frozen_now.unwrap_or_else(Instant::now);
    for id in ids {
        let drive = &state.drives[&id];
        let is_selected = selected == Some(id);
        let override_ssd = render_overrides.get(&id).copied();
        let action = draw_drive_panel(ui, drive, now, is_selected, override_ssd);
        if action.panel_clicked {
            clicked = Some(id);
        }
        if action.badge_clicked {
            // Cycle: None → Some(true SSD) → Some(false HDD) → None.
            let next = match render_overrides.get(&id).copied() {
                None => Some(true),
                Some(true) => Some(false),
                Some(false) => None,
            };
            if let Some(v) = next {
                render_overrides.insert(id, v);
            } else {
                render_overrides.remove(&id);
            }
            // Persist the new value (or its removal) keyed by the
            // drive's volume GUID. The GUID is stable across mounts,
            // unlike the drive letter — so an external drive that
            // got "A:" today and "F:" tomorrow keeps the override.
            // We swallow errors silently: a missing AppData write
            // is non-fatal for this scan, and the in-memory map is
            // still right for the current session.
            if !drive.info.volume_guid.is_empty() {
                let _ = crate::gui::drive_overrides::set(&drive.info.volume_guid, next);
            }
        }
        ui.add_space(8.0);
    }
    clicked
}

#[derive(Default)]
struct PanelAction {
    panel_clicked: bool,
    badge_clicked: bool,
}

/// Returns the panel + badge click state. `override_ssd` reflects the
/// app-level manual override: `None` use auto, `Some(true)` force SSD
/// render, `Some(false)` force HDD render. The badge cycles the
/// override on click.
fn draw_drive_panel(
    ui: &mut Ui,
    drive: &DriveLive,
    now: Instant,
    selected: bool,
    override_ssd: Option<bool>,
) -> PanelAction {
    let mut action = PanelAction::default();
    // Effective render mode: explicit override wins, otherwise use
    // the detection result (has_seek_penalty == true ⇒ HDD render).
    let effective_hdd = match override_ssd {
        Some(true) => false, // forced SSD render
        Some(false) => true, // forced HDD render
        None => drive.info.has_seek_penalty,
    };
    let stroke = if selected {
        Stroke::new(2.0, theme::ACCENT)
    } else {
        Stroke::new(1.0, Color32::from_rgb(0x1f, 0x28, 0x36))
    };
    let frame = egui::Frame::none()
        .fill(theme::PANEL_DEEP)
        .inner_margin(8.0)
        .rounding(6.0)
        .stroke(stroke);

    let _inner = frame.show(ui, |ui| {
        let mbps = drive.current_mbps();
        let type_color = if effective_hdd { theme::HDD } else { theme::SSD };
        let detected_label = if drive.info.has_seek_penalty { "HDD" } else { "SSD" };
        let badge_label = match override_ssd {
            Some(true) => format!("● SSD (manual)  [auto: {detected_label}]"),
            Some(false) => format!("● HDD (manual)  [auto: {detected_label}]"),
            None => format!("● {detected_label}"),
        };

        // Title row — this row IS the panel-click target. The
        // sparkline + LCN trace below get their own hover tooltips
        // independently. Splitting it this way means the panel-
        // level click sense doesn't overlap the trace rect, so the
        // trace's `Sense::hover()` response wins on its own area
        // and the tooltip pops up.
        let title_row = ui.horizontal(|ui| {
            let badge = ui.add(
                egui::Label::new(
                    RichText::new(badge_label).color(type_color).strong(),
                )
                .sense(egui::Sense::click()),
            );
            if badge
                .on_hover_text(
                    "Click to cycle render style: detected → SSD (scatter) → HDD (diagonal) → detected.",
                )
                .clicked()
            {
                action.badge_clicked = true;
            }
            ui.label(RichText::new(&drive.info.model).color(theme::TEXT_HI));
            ui.label(
                RichText::new(format!("/ {}", drive.info.volume_label)).color(theme::TEXT_LO),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{:>6.1} MB/s", mbps))
                        .color(theme::ACCENT)
                        .monospace()
                        .strong(),
                );
                ui.label(
                    RichText::new(format!("peak {:.0}", drive.peak_mbps))
                        .color(theme::TEXT_LO)
                        .monospace()
                        .small(),
                );
            });
        });
        // Reserve the title row's rect for the filter-toggle click.
        // The badge inside still wins over this larger rect because
        // it registered Sense::click() first as part of the
        // ui.add(Label) call above.
        let row_click = ui.interact(
            title_row.response.rect,
            ui.id().with(("drive-title-click", drive.info.id)),
            egui::Sense::click(),
        );
        if row_click.clicked() && !action.badge_clicked {
            action.panel_clicked = true;
        }

        ui.add_space(2.0);
        draw_sparkline(ui, drive, now);
        ui.add_space(4.0);
        draw_lcn_trace(ui, drive, now, effective_hdd);
    });
    action
}

fn draw_sparkline(ui: &mut Ui, drive: &DriveLive, now: Instant) {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), SPARK_HEIGHT), Sense::hover());
    resp.on_hover_text(
        "Throughput sparkline — bytes-per-second the engine is reading \
         from this drive over the last 30 seconds. Right edge = now.",
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, theme::BG);

    if drive.throughput.is_empty() {
        return;
    }

    let max_mbps = drive
        .throughput
        .iter()
        .map(|(_, b)| *b as f32 / 1_048_576.0)
        .fold(1.0_f32, f32::max);

    let now_s = now;
    let window = THROUGHPUT_WINDOW_SECS as f32;

    let mut prev: Option<egui::Pos2> = None;
    for (t, bytes) in &drive.throughput {
        let age = now_s.saturating_duration_since(*t).as_secs_f32();
        let x_frac = 1.0 - (age / window).clamp(0.0, 1.0);
        let y_frac = (*bytes as f32 / 1_048_576.0) / max_mbps;
        let p = rect.left_bottom()
            + vec2(
                rect.width() * x_frac,
                -rect.height() * y_frac.clamp(0.0, 1.0),
            );
        if let Some(pp) = prev {
            painter.line_segment([pp, p], Stroke::new(1.5, theme::ACCENT));
        }
        prev = Some(p);
    }

    // Axis hint.
    painter.text(
        rect.right_top() + vec2(-4.0, 2.0),
        egui::Align2::RIGHT_TOP,
        format!("{:.0} MB/s", max_mbps),
        FontId::proportional(10.0),
        theme::TEXT_LO,
    );
}

fn draw_lcn_trace(ui: &mut Ui, drive: &DriveLive, now: Instant, render_as_hdd: bool) {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), SCOPE_HEIGHT), Sense::hover());
    // Plain-English explanations first; the technical name (LCN) is
    // useful for screenshots but isn't what most users need to read.
    let tip = if render_as_hdd {
        "What you're looking at: where on the disk the engine has \
         been reading from over the last few seconds.\n\n\
         X axis = time (right edge = right now, left edge = ~30s ago).\n\
         Y axis = byte position on the drive (top = start of drive, \
         bottom = end).\n\n\
         The yellow line climbing diagonally is the engine working \
         through files in disk order — that's what we want on a \
         spinning HDD because the read head doesn't have to jump \
         back and forth. A messy zig-zag would mean we're thrashing \
         the head and the scan would crawl."
    } else {
        "What you're looking at: where on the disk the engine has \
         been reading from over the last few seconds.\n\n\
         X axis = time (right edge = right now, left edge = ~30s ago).\n\
         Y axis = byte position on the drive (top = start, bottom = end).\n\n\
         The teal scatter (\"TV snow\") is the engine reading all \
         over the SSD at once — perfect for SSDs because they have \
         no read-head seek cost. The denser the spray, the more \
         parallel I/O is in flight. A neat diagonal line here would \
         mean we're under-using the SSD."
    };
    resp.on_hover_text(tip);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, theme::BG);

    painter.text(
        rect.left_top() + vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        "LCN ↑   time →",
        FontId::proportional(10.0),
        theme::TEXT_LO,
    );

    if drive.reads.is_empty() {
        return;
    }

    let window = THROUGHPUT_WINDOW_SECS as f32;
    // If the user is forcing SSD render on an HDD-detected drive (or
    // vice versa), the LCNs the engine recorded won't match the render
    // pattern (monotonic vs scattered). Re-bucket Y positions via a
    // cheap hash of `lcn_bytes` so the SSD-style render gets the
    // scattered cloud regardless of what the engine emitted.
    let mismatch = render_as_hdd != drive.info.has_seek_penalty;
    let effective_lcn = |raw: u64| -> u64 {
        if !mismatch {
            return raw;
        }
        if render_as_hdd {
            // Forcing HDD render on SSD data: collapse to a monotonic
            // sequence keyed by sample order. Approximate via the raw
            // value modulo a sensible max.
            raw
        } else {
            // Forcing SSD render on HDD data: hash the cumulative
            // bytes into a fixed-range bucket so the trace scatters.
            let mut x = raw.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 30;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 27;
            x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
            x % (4 * 1024 * 1024 * 1024 * 1024u64)
        }
    };

    let max_lcn = drive
        .reads
        .iter()
        .map(|r| effective_lcn(r.lcn_bytes))
        .max()
        .unwrap_or(1)
        .max(1) as f32;

    let dot_color = if render_as_hdd {
        theme::HDD
    } else {
        theme::SSD
    };

    let total = drive.reads.len();
    let max_dots = if render_as_hdd { 1024 } else { 768 };
    let stride = (total / max_dots).max(1);
    let alpha = if render_as_hdd { 0.85 } else { 0.35 };
    let radius = if render_as_hdd { 2.0 } else { 1.4 };

    for (i, r) in drive.reads.iter().enumerate() {
        if i % stride != 0 {
            continue;
        }
        let age = now.saturating_duration_since(r.at).as_secs_f32();
        if age > window {
            continue;
        }
        let x_frac = 1.0 - (age / window);
        let y_lcn = effective_lcn(r.lcn_bytes) as f32;
        let y_frac = y_lcn / max_lcn;
        let p = rect.left_bottom()
            + vec2(
                rect.width() * x_frac,
                -rect.height() * y_frac.clamp(0.0, 1.0),
            );
        painter.circle_filled(p, radius, dot_color.gamma_multiply(alpha));
    }

    if let (Some(first), Some(last)) = (drive.reads.front(), drive.reads.back()) {
        let first_lcn = effective_lcn(first.lcn_bytes);
        let last_lcn = effective_lcn(last.lcn_bytes);
        let span = last_lcn.max(first_lcn) - last_lcn.min(first_lcn);
        let traversed = if render_as_hdd {
            format!("sequential   Δ {} this window", theme::humansize(span))
        } else {
            format!("scattered    Δ {} this window", theme::humansize(span))
        };
        painter.text(
            rect.right_top() + vec2(-4.0, 2.0),
            egui::Align2::RIGHT_TOP,
            traversed,
            FontId::proportional(10.0),
            theme::TEXT_LO,
        );
    }
}
