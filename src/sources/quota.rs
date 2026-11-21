//! Agent usage windows from `quota-axi --no-credential-refresh --json`.
//! `--no-credential-refresh` matters: without it a read may renew an expired
//! vendor session, which is a write.

use crate::model::QuotaWindow;
use crate::timefmt;
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Report {
    /// When quota-axi made the report: the moment its pace describes.
    generated_at: Option<String>,
    providers: Vec<Provider>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Provider {
    provider: String,
    windows: Vec<Window>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Window {
    id: String,
    label: String,
    percent_remaining: Option<f64>,
    resets_at: Option<String>,
    pace: Option<Pace>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Pace {
    status: Option<String>,
    /// Percent left minus percent of the window's time left: below zero
    /// the window is being spent faster than its clock runs.
    reserve_percent_points: Option<f64>,
}

/// When a window `used` percent spent runs out at its average rate so far,
/// as of `at`, if that is before it resets. quota-axi's reserve is percent
/// left minus percent of time left, so the share of the window's time gone
/// is `used + reserve`, and the window's length follows from the time left
/// until `resets_at`.
fn runs_out_at(used: f64, reserve: f64, resets_at: i64, at: i64) -> Option<i64> {
    let gone = used + reserve;
    let left = 100.0 - gone;
    if used <= 0.0 || gone <= 0.0 || left <= 0.0 || resets_at <= at {
        return None;
    }
    let length = (resets_at - at) as f64 * 100.0 / left;
    let elapsed = length * gone / 100.0;
    let out = at + ((100.0 - used) * elapsed / used).round() as i64;
    (out < resets_at).then_some(out)
}

/// quota-axi reports what is left; the panel shows what is spent.
fn used_from_remaining(remaining: f64) -> f64 {
    (100.0 - remaining).clamp(0.0, 100.0)
}

pub fn parse(json: &str) -> Result<Vec<QuotaWindow>, String> {
    let r: Report = serde_json::from_str(json).map_err(|e| format!("invalid quota JSON: {e}"))?;
    let at = r.generated_at.as_deref().and_then(timefmt::parse_iso);
    Ok(r.providers
        .into_iter()
        .flat_map(|p| {
            let provider = p.provider;
            p.windows.into_iter().filter_map(move |w| {
                // An older quota-axi without ids: the label is the next best name.
                let id = if w.id.is_empty() {
                    w.label.clone()
                } else {
                    w.id
                };
                let pct_used = used_from_remaining(w.percent_remaining?);
                let resets_at = w.resets_at.as_deref().and_then(timefmt::parse_iso);
                let pace = w.pace.unwrap_or_default();
                let status = pace.status.filter(|s| s != "unknown");
                let runs_out = match (
                    status.as_deref(),
                    pace.reserve_percent_points,
                    resets_at,
                    at,
                ) {
                    (Some("ahead"), Some(reserve), Some(reset), Some(at)) => {
                        runs_out_at(pct_used, reserve, reset, at)
                    }
                    _ => None,
                };
                Some(QuotaWindow {
                    provider: provider.clone(),
                    id,
                    label: w.label,
                    pct_used,
                    resets_at,
                    pace: status,
                    runs_out_at: runs_out,
                })
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_remaining_to_used() {
        assert_eq!(used_from_remaining(100.0), 0.0);
        assert_eq!(used_from_remaining(0.0), 100.0);
        assert_eq!(used_from_remaining(89.0), 11.0);
        assert_eq!(used_from_remaining(-3.0), 100.0);
        assert_eq!(used_from_remaining(104.0), 0.0);
    }

    #[test]
    fn a_window_spent_faster_than_its_clock_runs_out_before_reset() {
        // 35h left is 21% of the window, so 79% of its time (131.7h) is
        // gone. 86% spent in that time leaves 14%, which lasts 21.4h.
        let at = 1_000_000;
        let reset = at + 35 * 3600;
        let out = runs_out_at(86.0, -7.0, reset, at).unwrap();
        assert!((out - (at + 77_163)).abs() < 5, "{}", out - at);
        // On or under pace it lasts until the reset.
        assert_eq!(runs_out_at(41.0, 45.9, reset, at), None);
        assert_eq!(runs_out_at(0.0, -1.0, reset, at), None);
        assert_eq!(runs_out_at(50.0, -5.0, at, at), None);
        let json = include_str!("../../tests/fixtures/quota.json").replacen(
            "\"percentRemaining\": 65,\n          \"pace\": {\n            \"status\": \"behind\",\n            \"reservePercentPoints\": 43.9769,",
            "\"percentRemaining\": 14,\n          \"pace\": {\n            \"status\": \"ahead\",\n            \"reservePercentPoints\": -7.0,",
            1,
        );
        let w = parse(&json).unwrap();
        let week = w.iter().find(|w| w.id == "seven_day").unwrap();
        assert_eq!(week.pace.as_deref(), Some("ahead"));
        let made = timefmt::parse_iso("2026-11-19T14:40:52.735Z").unwrap();
        let out = week.runs_out_at.unwrap();
        assert!(out > made && out < week.resets_at.unwrap());
        assert!(
            w.iter()
                .filter(|w| w.id != "seven_day")
                .all(|w| w.runs_out_at.is_none())
        );
    }

    #[test]
    fn parses_windows_and_skips_signed_out_providers() {
        let w = parse(include_str!("../../tests/fixtures/quota.json")).unwrap();
        assert_eq!(w.len(), 6);
        assert_eq!(w[0].provider, "claude");
        assert_eq!(w[0].id, "five_hour");
        assert_eq!(w[0].label, "session");
        assert_eq!(w[0].pct_used, 41.0);
        assert_eq!(w[0].pace.as_deref(), Some("behind"));
        assert!(w[0].resets_at.is_some());
        assert_eq!(w[3].provider, "copilot");
        assert_eq!(w[3].pace, None, "unknown pace is dropped");
        assert_eq!(w[5].id, "premium_interactions");
        assert_eq!(w[5].pct_used, 100.0);
        let none = parse("{\"providers\": [{\"provider\": \"codex\", \"windows\": []}]}").unwrap();
        assert!(none.is_empty());
        let old = parse("{\"providers\": [{\"provider\": \"x\", \"windows\": [{\"label\": \"day\", \"percentRemaining\": 5}]}]}").unwrap();
        assert_eq!(
            old[0].id, "day",
            "a window without an id is named by its label"
        );
    }
}
