//! `home-runner ui`: a native dashboard over the job ledger — money saved, job counts and
//! other job statistics. Read-only; it never talks to GitHub and works without a config.

mod app;
mod stats;

use std::path::PathBuf;

use anyhow::{Result, anyhow};

pub fn run(ledgers: Vec<PathBuf>, since: String) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("home-runner")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "home-runner",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, ledgers, &since)))),
    )
    .map_err(|e| anyhow!("starting the dashboard: {e}"))
}
