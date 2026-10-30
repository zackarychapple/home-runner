//! Small building blocks shared by the tabs, and the GUI's colours.

use eframe::egui::{
    self, Color32, CornerRadius, Frame, Margin, Pos2, Rect, Response, RichText, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};

use crate::state::Phase;
use crate::timefmt;

pub const OK: Color32 = Color32::from_rgb(63, 185, 80);
pub const WARN: Color32 = Color32::from_rgb(210, 153, 34);
pub const BAD: Color32 = Color32::from_rgb(248, 81, 73);
pub const INFO: Color32 = Color32::from_rgb(88, 166, 255);
pub const CHART: Color32 = Color32::from_rgb(88, 166, 255);
pub const MUTED: Color32 = Color32::from_rgb(139, 148, 158);
pub const BORDER: Color32 = Color32::from_rgb(48, 54, 61);
pub const SURFACE: Color32 = Color32::from_rgb(22, 27, 34);
pub const SUBTLE: Color32 = Color32::from_rgb(33, 38, 45);

pub fn phase_color(phase: Phase) -> Color32 {
    match phase {
        Phase::Busy => OK,
        Phase::Launching | Phase::Registering | Phase::Preparing | Phase::Finishing => INFO,
        Phase::Backoff => WARN,
        Phase::Idle | Phase::Stopped => MUTED,
    }
}

pub fn result_color(result: &str) -> Color32 {
    match result {
        "Succeeded" => OK,
        "Failed" | "TimedOut" | "VmCrashed" => BAD,
        "Interrupted" => WARN,
        _ => MUTED,
    }
}

pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(6)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A small coloured, rounded tag; `filled` tints the background.
pub fn pill(ui: &mut Ui, text: impl Into<String>, color: Color32, filled: bool) -> Response {
    Frame::new()
        .fill(if filled {
            color.gamma_multiply(0.15)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                dot(ui, color);
                ui.label(RichText::new(text.into()).size(11.5));
            });
        })
        .response
}

pub fn tag(ui: &mut Ui, text: impl Into<String>) -> Response {
    Frame::new()
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(4)
        .inner_margin(Margin::symmetric(5, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text.into()).size(11.0).color(MUTED));
        })
        .response
}

pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// An uppercase section heading with optional muted text on the right.
pub fn section(ui: &mut Ui, title: &str, right: Option<String>) {
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title.to_uppercase())
                .size(11.0)
                .strong()
                .color(MUTED),
        );
        if let Some(right) = right {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(right).size(11.5).color(MUTED));
            });
        }
    });
    ui.add_space(4.0);
}

pub fn kpi(ui: &mut Ui, label: &str, value: String, sub: String, value_color: Option<Color32>) {
    card(ui, |ui| {
        ui.label(RichText::new(label).size(11.5).color(MUTED));
        let mut v = RichText::new(value).size(22.0).strong();
        if let Some(c) = value_color {
            v = v.color(c);
        }
        ui.label(v);
        ui.label(RichText::new(sub).size(11.5).color(MUTED));
    });
}

/// A row of equal-width KPI cards.
pub fn kpis(ui: &mut Ui, items: Vec<(&str, String, String, Option<Color32>)>) {
    let n = items.len();
    ui.columns(n, |cols| {
        for (col, (label, value, sub, color)) in cols.iter_mut().zip(items) {
            kpi(col, label, value, sub, color);
        }
    });
}

pub fn progress(ui: &mut Ui, fraction: f32, color: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3, SUBTLE);
    let mut fill = rect;
    fill.set_width(rect.width() * fraction.clamp(0.0, 1.0));
    painter.rect_filled(fill, 3, color);
}

/// Utilization samples (0..1) as a line.
pub fn sparkline(ui: &mut Ui, values: &[f32], size: Vec2) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, BORDER),
    );
    if values.len() >= 2 {
        let step = rect.width() / (values.len() - 1) as f32;
        let points: Vec<Pos2> = values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                Pos2::new(
                    rect.left() + i as f32 * step,
                    rect.bottom() - 1.0 - v.clamp(0.0, 1.0) * (rect.height() - 2.0),
                )
            })
            .collect();
        painter.line(points, Stroke::new(1.5, CHART));
    } else if let Some(v) = values.first() {
        let y = rect.bottom() - v.clamp(0.0, 1.0) * rect.height();
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(1.5, CHART),
        );
    }
    response
}

/// Daily bars with a hover tooltip; `labels` names each bar.
pub fn bar_chart(ui: &mut Ui, values: &[f64], labels: &[String], height: f32) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter();
    painter.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, BORDER),
    );
    if values.is_empty() {
        return;
    }
    let max = values.iter().copied().fold(0.0_f64, f64::max).max(0.01);
    let gap = 4.0;
    let bar_w = (rect.width() - gap * (values.len() - 1) as f32) / values.len() as f32;
    let hover = response.hover_pos();
    for (i, v) in values.iter().enumerate() {
        let x = rect.left() + i as f32 * (bar_w + gap);
        let h = (*v / max) as f32 * (rect.height() - 2.0);
        let bar = Rect::from_min_max(
            Pos2::new(x, rect.bottom() - h.max(1.0)),
            Pos2::new(x + bar_w, rect.bottom()),
        );
        let column = Rect::from_min_max(
            Pos2::new(x, rect.top()),
            Pos2::new(x + bar_w + gap, rect.bottom()),
        );
        let hovered = hover.is_some_and(|p| column.contains(p));
        painter.rect_filled(
            bar,
            CornerRadius {
                nw: 3,
                ne: 3,
                sw: 0,
                se: 0,
            },
            if hovered {
                CHART
            } else {
                CHART.gamma_multiply(0.8)
            },
        );
        if hovered {
            let label = labels.get(i).cloned().unwrap_or_default();
            response
                .clone()
                .on_hover_text(format!("{label}: {} saved", usd(*v)));
        }
    }
}

/// A box drawn with a dashed-looking outline: used for offline slots.
pub fn cell(ui: &mut Ui, fill: Option<Color32>, tooltip: impl Into<String>) {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
    match fill {
        Some(c) => {
            ui.painter().rect_filled(rect, 2, c);
        }
        None => {
            ui.painter()
                .rect_stroke(rect, 2, Stroke::new(1.0, BORDER), StrokeKind::Inside);
        }
    }
    response.on_hover_text(tooltip.into());
}

pub fn initials(name: &str) -> String {
    name.chars().take(2).collect::<String>().to_uppercase()
}

pub fn avatar(ui: &mut Ui, name: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 14.0, SUBTLE);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initials(name),
        egui::FontId::proportional(11.0),
        Color32::from_gray(220),
    );
}

pub fn usd(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("${:.0}", v)
    } else {
        format!("${v:.2}")
    }
}

pub fn dur(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

pub fn ago(ts: i64) -> String {
    let d = (timefmt::now() - ts).max(0);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// Opens a folder in Finder / Explorer.
pub fn reveal(path: &std::path::Path) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(path).spawn();
}
