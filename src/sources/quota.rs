//! Agent usage windows from `quota-axi --no-credential-refresh --json`.
//! `--no-credential-refresh` matters: without it a read may renew an expired
//! vendor session, which is a write.

use crate::model::QuotaWindow;
use crate::timefmt;
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(default)]
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
#[serde(default)]
struct Pace {
    status: Option<String>,
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
                Some(QuotaWindow {
                    provider: provider.clone(),
                    id,
                    label: w.label,
                    pct_used: used_from_remaining(w.percent_remaining?),
                    resets_at: w.resets_at.as_deref().and_then(timefmt::parse_iso),
                    pace: w.pace.and_then(|p| p.status).filter(|s| s != "unknown"),
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
