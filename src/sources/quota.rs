//! Agent usage windows from `quota-axi --no-credential-refresh --json --full`
//! (`--full` adds each window's projected run-out time to its pace).
//! `--no-credential-refresh` matters: without it a read may renew an expired
//! vendor session, which is a write.

use crate::model::QuotaWindow;
use crate::timefmt;
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Report {
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
    /// When the window runs out at its average rate so far, as quota-axi
    /// projects it (it gives one for windows that will last, too).
    projected_exhausted_at: Option<String>,
    /// `early` before a tenth of the window has passed, when a little use
    /// reads as a fast pace; `established` after.
    projection_confidence: Option<String>,
}

/// quota-axi reports what is left; the panel shows what is spent.
fn used_from_remaining(remaining: f64) -> f64 {
    (100.0 - remaining).clamp(0.0, 100.0)
}

pub fn parse(json: &str) -> Result<Vec<QuotaWindow>, String> {
    let r: Report = serde_json::from_str(json).map_err(|e| format!("invalid quota JSON: {e}"))?;
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
                // Spent faster than its clock, and far enough in for that to
                // be settled: quota-axi's own projection says when it runs
                // out, which is only news when that is before the reset.
                // (quota-axi projects a run-out for windows that will last
                // too, after their reset.)
                let projected = pace
                    .projected_exhausted_at
                    .as_deref()
                    .and_then(timefmt::parse_iso);
                let short = status.as_deref() == Some("ahead")
                    && pace.projection_confidence.as_deref() != Some("early")
                    && projected.is_none_or(|out| resets_at.is_none_or(|r| out < r));
                let runs_out = projected.filter(|_| short);
                Some(QuotaWindow {
                    provider: provider.clone(),
                    id,
                    label: w.label,
                    pct_used,
                    resets_at,
                    pace: status,
                    short,
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

    /// The fixture's week window, 65% left and `behind`, made `ahead`: 14%
    /// left, projected to run out at `out`, with `confidence`.
    fn week(out: &str, confidence: &str) -> QuotaWindow {
        let json = include_str!("../../tests/fixtures/quota.json")
            .replacen(
                "\"percentRemaining\": 65,\n          \"pace\": {\n            \"status\": \"behind\",",
                "\"percentRemaining\": 14,\n          \"pace\": {\n            \"status\": \"ahead\",",
                1,
            )
            .replacen("2026-11-22T06:11:43.000Z", out, 1)
            .replacen(
                "\"projectionConfidence\": \"established\"\n          }\n        },\n        {\n          \"id\": \"model:fable\"",
                &format!("\"projectionConfidence\": \"{confidence}\"\n          }}\n        }},\n        {{\n          \"id\": \"model:fable\""),
                1,
            );
        parse(&json)
            .unwrap()
            .into_iter()
            .find(|w| w.id == "seven_day")
            .unwrap()
    }

    #[test]
    fn the_run_out_time_is_quota_axis_own_projection() {
        // The week resets 2026-11-21T02:00Z. Spent faster than its clock and
        // projected to run out before: flagged, with quota-axi's time as it
        // gave it, not rebuilt from whole percents.
        let w = week("2026-11-20T21:18:42.000Z", "established");
        assert_eq!(w.pace.as_deref(), Some("ahead"));
        assert!(w.short);
        assert_eq!(w.runs_out_at, timefmt::parse_iso("2026-11-20T21:18:42Z"));
        // Early in the window a little use reads as a fast pace: not flagged.
        let early = week("2026-11-20T21:18:42.000Z", "early");
        assert_eq!((early.short, early.runs_out_at), (false, None));
        // A projection after the reset is not news, whatever the pace says.
        let after = week("2026-11-21T09:00:00.000Z", "established");
        assert_eq!((after.short, after.runs_out_at), (false, None));
        // Windows on or under pace carry a projection too (after their
        // reset) and are never flagged.
        let all = parse(include_str!("../../tests/fixtures/quota.json")).unwrap();
        assert!(all.iter().all(|w| !w.short && w.runs_out_at.is_none()));
        // A quota-axi without the projection fields still flags `ahead`.
        let old = parse(
            r#"{"providers": [{"provider": "x", "windows": [{"id": "d", "label": "day",
                "percentRemaining": 10, "pace": {"status": "ahead", "reservePercentPoints": -7.0}}]}]}"#,
        )
        .unwrap();
        assert_eq!((old[0].short, old[0].runs_out_at), (true, None));
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
