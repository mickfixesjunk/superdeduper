//! Pre-flight credit-report modal renderer.
//!
//! Aesthetic target per docs/preflight-spec.md: TransUnion / FICO
//! score-card surface. Big letter grade, three colored axis bars with
//! absolute MB/s numbers, recommendations list, [Cancel] / [Start scan]
//! footer.
//!
//! Returns `Some(PreflightAction)` exactly once when the user clicks a
//! footer button. The caller transitions state on receipt.

use egui::{Color32, Context, RichText, Stroke, Window};

use crate::diagnose::{DiagnoseReport, Recommendation, RecommendationImpact};
use crate::gui::preflight::{AxisScore, DiskAxis, DriveScore, Grade, PreflightAction, PreflightState};
use crate::gui::theme;

pub fn show(ctx: &Context, state: &PreflightState) -> Option<PreflightAction> {
    match state {
        PreflightState::Idle => None,
        PreflightState::Probing {
            started_at, roots, ..
        } => show_probing(ctx, started_at, roots),
        PreflightState::Showing { report, grade } => show_report(ctx, report, grade),
        PreflightState::Failed(err) => show_failed(ctx, err),
    }
}

fn show_probing(
    ctx: &Context,
    started_at: &std::time::Instant,
    roots: &[std::path::PathBuf],
) -> Option<PreflightAction> {
    let mut action: Option<PreflightAction> = None;
    let elapsed = started_at.elapsed().as_secs_f32();
    Window::new(
        RichText::new("Pre-flight check")
            .color(theme::TEXT_HI)
            .heading(),
    )
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
    .default_width(480.0)
    .show(ctx, |ui| {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add_space(8.0);
            ui.label(
                RichText::new("Probing your machine…")
                    .color(theme::TEXT_HI)
                    .size(14.0),
            );
        });
        ui.add_space(6.0);
        for r in roots {
            ui.label(
                RichText::new(format!("Target: {}", r.display()))
                    .color(theme::TEXT_LO)
                    .small()
                    .monospace(),
            );
        }
        ui.label(
            RichText::new(format!("Elapsed: {:.1}s", elapsed))
                .color(theme::TEXT_LO)
                .small(),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Measuring hash compute, plus Tier 1 + Tier 3 disk throughput for each \
                 drive in the scan. Usually 5–15 seconds per drive.",
            )
            .color(theme::TEXT_LO)
            .small()
            .italics(),
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Cancel scan").color(theme::TEXT_HI))
                        .min_size(egui::vec2(110.0, 28.0)),
                )
                .clicked()
            {
                action = Some(PreflightAction::Cancel);
            }
            ui.add_space(8.0);
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("Skip pre-flight  →")
                            .color(theme::PANEL_DEEP)
                            .strong(),
                    )
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(170.0, 28.0)),
                )
                .clicked()
            {
                action = Some(PreflightAction::Start);
            }
        });
    });
    ctx.request_repaint_after(std::time::Duration::from_millis(80));
    action
}

fn show_failed(ctx: &Context, err: &str) -> Option<PreflightAction> {
    let mut action: Option<PreflightAction> = None;
    Window::new(
        RichText::new("Pre-flight check failed")
            .color(theme::WARN)
            .heading(),
    )
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
    .default_width(480.0)
    .show(ctx, |ui| {
        ui.add_space(6.0);
        ui.label(
            RichText::new(err)
                .color(theme::TEXT_HI)
                .size(13.0)
                .monospace(),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new("You can still start the scan — pre-flight is advisory.")
                .color(theme::TEXT_LO)
                .small(),
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Cancel").color(theme::TEXT_HI))
                        .min_size(egui::vec2(100.0, 28.0)),
                )
                .clicked()
            {
                action = Some(PreflightAction::Cancel);
            }
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("Start scan →")
                            .color(theme::PANEL_DEEP)
                            .strong(),
                    )
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(140.0, 28.0)),
                )
                .clicked()
            {
                action = Some(PreflightAction::Start);
            }
        });
    });
    action
}

fn show_report(
    ctx: &Context,
    report: &DiagnoseReport,
    grade: &Grade,
) -> Option<PreflightAction> {
    let mut action: Option<PreflightAction> = None;
    Window::new(
        RichText::new("superdeduper · PREFLIGHT")
            .color(theme::TEXT_LO)
            .small()
            .strong(),
    )
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
    .default_width(560.0)
    .show(ctx, |ui| {
        ui.add_space(6.0);

        // Score card row: big letter on the left, "Scan readiness"
        // label on the right.
        ui.horizontal(|ui| {
            draw_grade_chip(ui, grade);
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Scan readiness")
                        .color(theme::TEXT_HI)
                        .size(18.0)
                        .strong(),
                );
                ui.label(
                    RichText::new(format!("{}% overall · {} cores · river5 {}",
                        grade.overall_percent,
                        report.system.cpu_threads,
                        report.system.river5_impl))
                        .color(theme::TEXT_LO)
                        .small(),
                );
            });
        });

        ui.add_space(14.0);
        ui.separator();
        ui.add_space(8.0);

        axis_row(
            ui,
            "Hardware capability",
            &grade.hardware,
            &format!(
                "{:.0} MB/s single-stream (river5)",
                report.hash.river5_single_thread_mbps
            ),
        );
        ui.add_space(8.0);
        disk_axis_rows(ui, &grade.disk);
        ui.add_space(8.0);
        axis_row(
            ui,
            "Hash compute",
            &grade.hash,
            &format!(
                "{:.0} MB/s aggregate (river5 × {} threads)",
                report.hash.river5_aggregate_mbps, report.system.cpu_threads
            ),
        );

        ui.add_space(14.0);
        ui.separator();
        ui.add_space(6.0);

        // Recommendations from the engine. Group by impact.
        for impact in [
            RecommendationImpact::High,
            RecommendationImpact::Medium,
            RecommendationImpact::Low,
            RecommendationImpact::Informational,
        ] {
            for rec in report
                .recommendations
                .iter()
                .filter(|r| std::mem::discriminant(&r.impact) == std::mem::discriminant(&impact))
            {
                render_recommendation(ui, rec);
            }
        }

        if report.recommendations.is_empty() {
            ui.label(
                RichText::new("No recommendations — you're good to go.")
                    .color(theme::TEXT_LO)
                    .italics()
                    .small(),
            );
        }

        ui.add_space(14.0);
        ui.separator();
        ui.add_space(6.0);

        // Footer buttons.
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("Cancel").color(theme::TEXT_HI))
                        .min_size(egui::vec2(100.0, 30.0)),
                )
                .clicked()
            {
                action = Some(PreflightAction::Cancel);
            }
            ui.add_space(8.0);
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("Start scan →")
                            .color(theme::PANEL_DEEP)
                            .strong(),
                    )
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(160.0, 30.0)),
                )
                .clicked()
            {
                action = Some(PreflightAction::Start);
            }
        });
    });
    action
}

fn draw_grade_chip(ui: &mut egui::Ui, grade: &Grade) {
    let size = egui::vec2(88.0, 88.0);
    let (rect, _resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let bg = color_for_percent(grade.overall_percent);
    painter.rect(
        rect,
        12.0,
        theme::PANEL_DEEP,
        Stroke::new(2.5, bg),
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        grade.letter.to_string(),
        egui::FontId::new(48.0, egui::FontFamily::Proportional),
        bg,
    );
}

fn disk_axis_rows(ui: &mut egui::Ui, disk: &DiskAxis) {
    let composite_pct = disk.composite_percent.unwrap_or(0);
    let composite_color = match disk.composite_percent {
        Some(p) => color_for_percent(p),
        None => theme::TEXT_LO,
    };
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Disk read")
                    .color(theme::TEXT_HI)
                    .strong()
                    .size(13.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match disk.composite_percent {
                    Some(p) => {
                        ui.label(
                            RichText::new(format!("{}%", p))
                                .color(composite_color)
                                .strong()
                                .size(13.0),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(format!(
                                "composite of {} measured drive(s)",
                                disk.drives.iter().filter(|d| d.percent.is_some()).count()
                            ))
                            .color(theme::TEXT_LO)
                            .size(12.0),
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new("not measured")
                                .color(theme::WARN)
                                .strong()
                                .size(13.0),
                        );
                    }
                }
            });
        });
        ui.add_space(2.0);
        if disk.composite_percent.is_some() {
            draw_bar(ui, composite_pct, composite_color);
        }
        ui.add_space(4.0);

        for drive in &disk.drives {
            drive_row(ui, drive);
        }
    });
}

fn drive_row(ui: &mut egui::Ui, drive: &DriveScore) {
    ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(
            RichText::new(format!("• {}", drive.identifier))
                .color(theme::TEXT_HI)
                .size(12.0)
                .monospace(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match (drive.percent, drive.tier3_mbps) {
                (Some(p), Some(mbps)) => {
                    let color = color_for_percent(p);
                    ui.label(
                        RichText::new(format!("{}%", p))
                            .color(color)
                            .size(12.0)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!("{:.0} MB/s", mbps))
                            .color(theme::TEXT_LO)
                            .size(11.0)
                            .monospace(),
                    );
                }
                _ => {
                    let note = drive
                        .error
                        .as_deref()
                        .unwrap_or("not measured");
                    ui.label(
                        RichText::new(format!("({})", note))
                            .color(theme::TEXT_LO)
                            .size(11.0)
                            .italics(),
                    );
                }
            }
        });
    });
    if let Some(p) = drive.percent {
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            let color = color_for_percent(p);
            let height = 6.0;
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), height),
                egui::Sense::hover(),
            );
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 2.0, theme::PANEL_DEEP);
            let fill_width = rect.width() * (p as f32 / 100.0);
            let fill_rect =
                egui::Rect::from_min_size(rect.min, egui::vec2(fill_width, rect.height()));
            painter.rect_filled(fill_rect, 2.0, color);
        });
    }
    ui.add_space(2.0);
}

fn axis_row(ui: &mut egui::Ui, label: &str, score: &AxisScore, raw: &str) {
    let color = color_for_percent(score.percent);
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(label)
                    .color(theme::TEXT_HI)
                    .strong()
                    .size(13.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{}%", score.percent))
                        .color(color)
                        .strong()
                        .size(13.0),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(raw)
                        .color(theme::TEXT_LO)
                        .size(12.0)
                        .monospace(),
                );
            });
        });
        ui.add_space(2.0);
        draw_bar(ui, score.percent, color);
    });
}

fn draw_bar(ui: &mut egui::Ui, percent: u8, color: Color32) {
    let height = 10.0;
    let (rect, _resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, theme::PANEL_DEEP);
    let fill_width = rect.width() * (percent as f32 / 100.0);
    let fill_rect = egui::Rect::from_min_size(rect.min, egui::vec2(fill_width, rect.height()));
    painter.rect_filled(fill_rect, 3.0, color);
}

fn render_recommendation(ui: &mut egui::Ui, rec: &Recommendation) {
    let (icon, icon_color) = match rec.impact {
        RecommendationImpact::High => ("⚠", theme::HOT),
        RecommendationImpact::Medium => ("⚠", theme::WARN),
        RecommendationImpact::Low => ("ℹ", theme::COOL),
        RecommendationImpact::Informational => ("ℹ", theme::TEXT_LO),
    };
    ui.horizontal_top(|ui| {
        ui.label(RichText::new(icon).color(icon_color).size(14.0).strong());
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new(&rec.title)
                    .color(theme::TEXT_HI)
                    .size(13.0)
                    .strong(),
            );
            ui.label(
                RichText::new(&rec.detail)
                    .color(theme::TEXT_LO)
                    .size(12.0),
            );
        });
    });
    ui.add_space(6.0);
}

fn color_for_percent(p: u8) -> Color32 {
    match p {
        80..=100 => theme::ACCENT, // green-teal
        50..=79 => theme::WARN,    // amber
        _ => theme::HOT,           // red-pink
    }
}
