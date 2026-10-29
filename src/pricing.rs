//! List prices for comparable hosted runners, used to estimate what each job would have cost.
//!
//! Sources (checked 2026-10-29):
//! - GitHub: https://docs.github.com/en/billing/reference/actions-runner-pricing
//!   Each job is rounded up to a whole minute.
//! - Depot:  https://depot.dev/docs/github-actions/runner-types and https://depot.dev/pricing
//!   Billed per second.
//!
//! Prices change; override any of them in `[pricing.rates]` without a new release.

use serde::{Deserialize, Serialize};

use crate::config::PricingConfig;

pub const PRICES_AS_OF: &str = "2026-10-29";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    GitHub,
    Depot,
}

#[derive(Debug)]
pub struct Sku {
    pub provider: Provider,
    /// What you put in `compare_github` / `compare_depot` and `[pricing.rates]`.
    pub id: &'static str,
    pub label: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
    pub cpus: u32,
    pub usd_per_minute: f64,
    /// GitHub's standard runners cost nothing for public repositories.
    pub free_for_public: bool,
}

const fn gh(
    id: &'static str,
    label: &'static str,
    os: &'static str,
    arch: &'static str,
    cpus: u32,
    usd: f64,
    standard: bool,
) -> Sku {
    Sku {
        provider: Provider::GitHub,
        id,
        label,
        os,
        arch,
        cpus,
        usd_per_minute: usd,
        free_for_public: standard,
    }
}

const fn depot(
    id: &'static str,
    label: &'static str,
    os: &'static str,
    arch: &'static str,
    cpus: u32,
    usd: f64,
) -> Sku {
    Sku {
        provider: Provider::Depot,
        id,
        label,
        os,
        arch,
        cpus,
        usd_per_minute: usd,
        free_for_public: false,
    }
}

#[rustfmt::skip]
pub static SKUS: &[Sku] = &[
    gh("github-linux-x64-2",    "GitHub Linux 2-core x64",          "Linux",   "X64",   2,  0.006, true),
    gh("github-linux-x64-4",    "GitHub Linux 4-core x64",          "Linux",   "X64",   4,  0.012, false),
    gh("github-linux-x64-8",    "GitHub Linux 8-core x64",          "Linux",   "X64",   8,  0.022, false),
    gh("github-linux-x64-16",   "GitHub Linux 16-core x64",         "Linux",   "X64",   16, 0.042, false),
    gh("github-linux-x64-32",   "GitHub Linux 32-core x64",         "Linux",   "X64",   32, 0.082, false),
    gh("github-linux-x64-64",   "GitHub Linux 64-core x64",         "Linux",   "X64",   64, 0.162, false),
    gh("github-linux-x64-96",   "GitHub Linux 96-core x64",         "Linux",   "X64",   96, 0.252, false),
    gh("github-linux-arm64-2",  "GitHub Linux 2-core arm64",        "Linux",   "ARM64", 2,  0.005, true),
    gh("github-linux-arm64-4",  "GitHub Linux 4-core arm64",        "Linux",   "ARM64", 4,  0.008, false),
    gh("github-linux-arm64-8",  "GitHub Linux 8-core arm64",        "Linux",   "ARM64", 8,  0.014, false),
    gh("github-linux-arm64-16", "GitHub Linux 16-core arm64",       "Linux",   "ARM64", 16, 0.026, false),
    gh("github-linux-arm64-32", "GitHub Linux 32-core arm64",       "Linux",   "ARM64", 32, 0.050, false),
    gh("github-linux-arm64-64", "GitHub Linux 64-core arm64",       "Linux",   "ARM64", 64, 0.098, false),
    gh("github-windows-x64-2",  "GitHub Windows 2-core x64",        "Windows", "X64",   2,  0.010, true),
    gh("github-windows-x64-4",  "GitHub Windows 4-core x64",        "Windows", "X64",   4,  0.022, false),
    gh("github-windows-x64-8",  "GitHub Windows 8-core x64",        "Windows", "X64",   8,  0.042, false),
    gh("github-windows-x64-16", "GitHub Windows 16-core x64",       "Windows", "X64",   16, 0.082, false),
    gh("github-windows-x64-32", "GitHub Windows 32-core x64",       "Windows", "X64",   32, 0.162, false),
    gh("github-windows-x64-64", "GitHub Windows 64-core x64",       "Windows", "X64",   64, 0.322, false),
    gh("github-windows-x64-96", "GitHub Windows 96-core x64",       "Windows", "X64",   96, 0.552, false),
    gh("github-windows-arm64-2","GitHub Windows 2-core arm64",      "Windows", "ARM64", 2,  0.010, true),
    gh("github-windows-arm64-4","GitHub Windows 4-core arm64",      "Windows", "ARM64", 4,  0.014, false),
    gh("github-windows-arm64-8","GitHub Windows 8-core arm64",      "Windows", "ARM64", 8,  0.026, false),
    gh("github-macos-arm64-3",  "GitHub macOS 3-core M1",           "macOS",   "ARM64", 3,  0.062, true),
    gh("github-macos-arm64-5",  "GitHub macOS 5-core M2 Pro",       "macOS",   "ARM64", 5,  0.102, false),
    gh("github-macos-x64-4",    "GitHub macOS 4-core Intel",        "macOS",   "X64",   4,  0.062, true),
    gh("github-macos-x64-12",   "GitHub macOS 12-core Intel",       "macOS",   "X64",   12, 0.077, false),

    depot("depot-ubuntu-24.04",         "Depot Linux 2-vCPU x64",    "Linux",   "X64",   2,  0.006),
    depot("depot-ubuntu-24.04-4",       "Depot Linux 4-vCPU x64",    "Linux",   "X64",   4,  0.012),
    depot("depot-ubuntu-24.04-8",       "Depot Linux 8-vCPU x64",    "Linux",   "X64",   8,  0.024),
    depot("depot-ubuntu-24.04-16",      "Depot Linux 16-vCPU x64",   "Linux",   "X64",   16, 0.048),
    depot("depot-ubuntu-24.04-32",      "Depot Linux 32-vCPU x64",   "Linux",   "X64",   32, 0.096),
    depot("depot-ubuntu-24.04-64",      "Depot Linux 64-vCPU x64",   "Linux",   "X64",   64, 0.192),
    depot("depot-ubuntu-24.04-arm",     "Depot Linux 2-vCPU arm64",  "Linux",   "ARM64", 2,  0.006),
    depot("depot-ubuntu-24.04-arm-4",   "Depot Linux 4-vCPU arm64",  "Linux",   "ARM64", 4,  0.012),
    depot("depot-ubuntu-24.04-arm-8",   "Depot Linux 8-vCPU arm64",  "Linux",   "ARM64", 8,  0.024),
    depot("depot-ubuntu-24.04-arm-16",  "Depot Linux 16-vCPU arm64", "Linux",   "ARM64", 16, 0.048),
    depot("depot-ubuntu-24.04-arm-32",  "Depot Linux 32-vCPU arm64", "Linux",   "ARM64", 32, 0.096),
    depot("depot-ubuntu-24.04-arm-64",  "Depot Linux 64-vCPU arm64", "Linux",   "ARM64", 64, 0.192),
    depot("depot-windows-2025",         "Depot Windows 2-vCPU",      "Windows", "X64",   2,  0.008),
    depot("depot-windows-2025-4",       "Depot Windows 4-vCPU",      "Windows", "X64",   4,  0.016),
    depot("depot-windows-2025-8",       "Depot Windows 8-vCPU",      "Windows", "X64",   8,  0.032),
    depot("depot-windows-2025-16",      "Depot Windows 16-vCPU",     "Windows", "X64",   16, 0.064),
    depot("depot-windows-2025-32",      "Depot Windows 32-vCPU",     "Windows", "X64",   32, 0.128),
    depot("depot-windows-2025-64",      "Depot Windows 64-vCPU",     "Windows", "X64",   64, 0.256),
    depot("depot-macos-latest",         "Depot macOS 8-vCPU M-series", "macOS", "ARM64", 8,  0.080),
];

pub fn find(id: &str) -> Option<&'static Sku> {
    SKUS.iter().find(|s| s.id == id)
}

/// The smallest SKU with at least `cpus` cores for this OS (same arch when there is one).
pub fn closest(provider: Provider, os: &str, arch: &str, cpus: u32) -> Option<&'static Sku> {
    let for_os = || {
        SKUS.iter()
            .filter(move |s| s.provider == provider && s.os == os)
    };
    let same_arch: Vec<_> = for_os().filter(|s| s.arch == arch).collect();
    let candidates = if same_arch.is_empty() {
        for_os().collect()
    } else {
        same_arch
    };
    candidates
        .iter()
        .filter(|s| s.cpus >= cpus)
        .min_by_key(|s| s.cpus)
        .or_else(|| candidates.iter().max_by_key(|s| s.cpus))
        .copied()
}

/// What one job would have cost on a hosted runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comparison {
    pub sku: String,
    pub label: String,
    pub usd_per_minute: f64,
    pub billed_minutes: f64,
    pub usd: f64,
}

pub struct Pricing {
    cfg: PricingConfig,
}

impl Pricing {
    pub fn new(cfg: PricingConfig) -> Self {
        Self { cfg }
    }

    pub fn rate(&self, sku: &Sku) -> f64 {
        self.cfg
            .rates
            .get(sku.id)
            .copied()
            .unwrap_or(sku.usd_per_minute)
    }

    pub fn compare(&self, sku: &Sku, duration_secs: f64, public_repo: bool) -> Comparison {
        let billed_minutes = match sku.provider {
            Provider::GitHub => (duration_secs / 60.0).ceil().max(1.0),
            Provider::Depot => duration_secs / 60.0,
        };
        let rate = if public_repo && sku.free_for_public {
            0.0
        } else {
            self.rate(sku)
        };
        Comparison {
            sku: sku.id.to_string(),
            label: sku.label.to_string(),
            usd_per_minute: rate,
            billed_minutes,
            usd: round_cents(billed_minutes * rate),
        }
    }

    /// Electricity for the job plus any per-minute fee GitHub charges for self-hosted runners.
    pub fn own_cost(&self, duration_secs: f64) -> (f64, f64) {
        let kwh = self.cfg.job_watts * duration_secs / 3600.0 / 1000.0;
        let electricity = kwh * self.cfg.electricity_usd_per_kwh;
        let fee = (duration_secs / 60.0).ceil() * self.cfg.github_self_hosted_usd_per_minute;
        (round_cents(electricity), round_cents(fee))
    }
}

/// Rounded to a hundredth of a cent so ledgers stay readable but totals stay accurate.
pub fn round_cents(usd: f64) -> f64 {
    (usd * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_smallest_sku_that_fits() {
        assert_eq!(
            closest(Provider::GitHub, "Linux", "ARM64", 4).unwrap().id,
            "github-linux-arm64-4"
        );
        assert_eq!(
            closest(Provider::GitHub, "Linux", "ARM64", 3).unwrap().id,
            "github-linux-arm64-4"
        );
        assert_eq!(
            closest(Provider::GitHub, "macOS", "ARM64", 4).unwrap().id,
            "github-macos-arm64-5"
        );
        assert_eq!(
            closest(Provider::GitHub, "Linux", "X64", 500).unwrap().id,
            "github-linux-x64-96"
        );
        // Depot has no Intel Macs, so fall back to what it does have.
        assert_eq!(
            closest(Provider::Depot, "macOS", "X64", 4).unwrap().id,
            "depot-macos-latest"
        );
        assert!(SKUS.iter().all(|s| find(s.id).is_some()));
    }

    #[test]
    fn github_rounds_up_per_job_and_depot_bills_per_second() {
        let pricing = Pricing::new(PricingConfig::default());
        let gh = pricing.compare(find("github-linux-arm64-4").unwrap(), 61.0, false);
        assert_eq!((gh.billed_minutes, gh.usd), (2.0, 0.016));
        let depot = pricing.compare(find("depot-ubuntu-24.04-arm-4").unwrap(), 90.0, false);
        assert_eq!((depot.billed_minutes, depot.usd), (1.5, 0.018));
        let public = pricing.compare(find("github-linux-x64-2").unwrap(), 600.0, true);
        assert_eq!(public.usd, 0.0);
    }
}
