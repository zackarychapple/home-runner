//! Aggregates the dashboard shows: totals, result breakdown, duration/queue percentiles and a
//! per-day series. Pure functions over ledger records so they can be tested without a window.

use std::collections::BTreeMap;

use crate::ledger::{GroupBy, JobRecord, Totals};

const DAY: i64 = 86_400;

#[derive(Default, Clone, Copy)]
pub struct Percentiles {
    pub avg: f64,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

impl Percentiles {
    fn of(mut values: Vec<f64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(f64::total_cmp);
        let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
        Some(Self {
            avg: values.iter().sum::<f64>() / values.len() as f64,
            median: at(0.5),
            p95: at(0.95),
            max: values[values.len() - 1],
        })
    }
}

#[derive(Default, Clone, Copy)]
pub struct DayPoint {
    /// Start of the UTC day, in days since the Unix epoch.
    pub day: i64,
    pub succeeded: usize,
    pub failed: usize,
    pub other: usize,
    pub saved_github: f64,
    pub saved_depot: f64,
    /// Running totals up to and including this day.
    pub cum_saved_github: f64,
    pub cum_saved_depot: f64,
}

pub struct Group {
    pub key: String,
    pub totals: Totals,
}

pub struct Stats {
    pub totals: Totals,
    pub succeeded: usize,
    /// Failed or timed out.
    pub failed: usize,
    /// Canceled, interrupted or unknown.
    pub other: usize,
    pub duration: Option<Percentiles>,
    pub queue: Option<Percentiles>,
    pub daily: Vec<DayPoint>,
    pub first: Option<i64>,
    pub last: Option<i64>,
}

impl Stats {
    pub fn compute(records: &[&JobRecord]) -> Self {
        let mut totals = Totals::default();
        let (mut succeeded, mut failed, mut other) = (0, 0, 0);
        let mut by_day: BTreeMap<i64, DayPoint> = BTreeMap::new();
        for r in records {
            totals.add(r);
            let point = by_day.entry(r.started_at.div_euclid(DAY)).or_default();
            match r.result.as_str() {
                "Succeeded" => {
                    succeeded += 1;
                    point.succeeded += 1;
                }
                "Failed" | "TimedOut" => {
                    failed += 1;
                    point.failed += 1;
                }
                _ => {
                    other += 1;
                    point.other += 1;
                }
            }
            point.saved_github += r.saved_vs_github_usd.unwrap_or(0.0);
            point.saved_depot += r.saved_vs_depot_usd.unwrap_or(0.0);
        }

        // Fill quiet days so the charts keep a true time axis.
        let mut daily = Vec::new();
        if let (Some(&lo), Some(&hi)) = (by_day.keys().next(), by_day.keys().next_back()) {
            let (mut cum_gh, mut cum_depot) = (0.0, 0.0);
            for day in lo..=hi {
                let mut p = by_day.remove(&day).unwrap_or_default();
                p.day = day;
                cum_gh += p.saved_github;
                cum_depot += p.saved_depot;
                p.cum_saved_github = cum_gh;
                p.cum_saved_depot = cum_depot;
                daily.push(p);
            }
        }

        Self {
            totals,
            succeeded,
            failed,
            other,
            duration: Percentiles::of(records.iter().map(|r| r.duration_secs).collect()),
            queue: Percentiles::of(records.iter().filter_map(|r| r.queued_secs).collect()),
            daily,
            first: records.iter().map(|r| r.started_at).min(),
            last: records.iter().map(|r| r.started_at).max(),
        }
    }

    pub fn jobs(&self) -> usize {
        self.totals.jobs
    }

    /// Share of jobs that succeeded, 0.0–1.0.
    pub fn success_rate(&self) -> Option<f64> {
        (self.jobs() > 0).then(|| self.succeeded as f64 / self.jobs() as f64)
    }

    pub fn avg_utilization(&self) -> Option<f64> {
        (self.totals.capacity_secs > 0.0).then(|| self.totals.cpu_secs / self.totals.capacity_secs)
    }
}

/// Groups records and sorts them by money saved vs GitHub, biggest first (by date for day/month).
pub fn group(records: &[&JobRecord], by: GroupBy) -> Vec<Group> {
    let mut groups: BTreeMap<String, Totals> = BTreeMap::new();
    for r in records {
        groups.entry(by.key(r)).or_default().add(r);
    }
    let mut rows: Vec<Group> = groups
        .into_iter()
        .map(|(key, totals)| Group { key, totals })
        .collect();
    if !matches!(by, GroupBy::Day | GroupBy::Month) {
        rows.sort_by(|a, b| b.totals.saved_github.total_cmp(&a.totals.saved_github));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(day: i64, result: &str, repo: &str, secs: f64, saved: f64) -> JobRecord {
        JobRecord {
            id: format!("{day}-{repo}-{secs}"),
            repo: Some(repo.into()),
            result: result.into(),
            started_at: day * DAY + 3600,
            duration_secs: secs,
            queued_secs: Some(secs / 10.0),
            saved_vs_github_usd: Some(saved),
            saved_vs_depot_usd: Some(saved * 2.0),
            ..Default::default()
        }
    }

    #[test]
    fn counts_results_and_money() {
        let jobs = [
            job(100, "Succeeded", "a/x", 60.0, 0.5),
            job(100, "Failed", "a/x", 120.0, 0.25),
            job(102, "Canceled", "a/y", 30.0, 0.0),
            job(102, "TimedOut", "a/y", 90.0, 0.25),
        ];
        let refs: Vec<_> = jobs.iter().collect();
        let s = Stats::compute(&refs);
        assert_eq!((s.jobs(), s.succeeded, s.failed, s.other), (4, 1, 2, 1));
        assert_eq!(s.success_rate(), Some(0.25));
        assert!((s.totals.saved_github - 1.0).abs() < 1e-9);
        assert!((s.totals.saved_depot - 2.0).abs() < 1e-9);
    }

    #[test]
    fn daily_series_fills_gaps_and_accumulates() {
        let jobs = [
            job(100, "Succeeded", "a/x", 60.0, 1.0),
            job(103, "Succeeded", "a/x", 60.0, 2.0),
        ];
        let refs: Vec<_> = jobs.iter().collect();
        let s = Stats::compute(&refs);
        let days: Vec<_> = s
            .daily
            .iter()
            .map(|d| (d.day, d.succeeded + d.failed + d.other))
            .collect();
        assert_eq!(days, [(100, 1), (101, 0), (102, 0), (103, 1)]);
        let cum: Vec<_> = s.daily.iter().map(|d| d.cum_saved_github).collect();
        assert_eq!(cum, [1.0, 1.0, 1.0, 3.0]);
    }

    #[test]
    fn percentiles() {
        let p = Percentiles::of((1..=100).map(f64::from).collect()).unwrap();
        assert_eq!(p.avg, 50.5);
        assert_eq!(p.median, 51.0);
        assert_eq!(p.p95, 95.0);
        assert_eq!(p.max, 100.0);
        assert!(Percentiles::of(vec![]).is_none());
    }

    #[test]
    fn empty_input_is_safe() {
        let s = Stats::compute(&[]);
        assert_eq!(s.jobs(), 0);
        assert!(s.daily.is_empty() && s.success_rate().is_none() && s.duration.is_none());
    }

    #[test]
    fn groups_sort_by_savings() {
        let jobs = [
            job(100, "Succeeded", "a/small", 60.0, 0.1),
            job(100, "Succeeded", "a/big", 60.0, 5.0),
            job(101, "Succeeded", "a/big", 60.0, 5.0),
        ];
        let refs: Vec<_> = jobs.iter().collect();
        let rows = group(&refs, GroupBy::Repo);
        assert_eq!(rows[0].key, "a/big");
        assert_eq!(rows[0].totals.jobs, 2);
        assert_eq!(group(&refs, GroupBy::Day)[0].key, timefmt_day(100));
    }

    fn timefmt_day(day: i64) -> String {
        crate::timefmt::day(day * DAY)
    }
}
