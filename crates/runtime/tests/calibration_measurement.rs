//! Manual instrument: measures the token estimator's error against real
//! provider-reported usage from saved sessions.
//!
//! `#[ignore]`d on purpose — it reads a developer's session directory, so it has
//! nothing to assert in CI. Run it after touching `compact.rs`'s estimator or
//! `TokenCalibration` to confirm the correction still tracks reality:
//!
//! ```text
//! cargo test -p heartflow-runtime --test calibration_measurement -- --ignored --nocapture
//! HF_SESSION_DIR=/path/to/sessions cargo test ... (to point elsewhere)
//! ```
//!
//! Recorded baseline on 2026-09-21 over 451 turns from 28 saved sessions:
//! raw heuristic median 62.4% / bias -57.5%, calibrated median 8.9% / bias
//! -1.5%, learned density ~2.23, learned fixed overhead ~11.6k tokens.

use std::fs;

use runtime::{estimate_tokens_from, Session, TokenCalibration};

/// Mirrors `MIN_SAMPLE_TOKENS` in `compact.rs`: a smaller prompt says more about
/// per-message framing than about tokenization.
const MIN_SAMPLE_TOKENS: usize = 256;

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    sorted[(sorted.len() as f64 * fraction) as usize]
}

#[test]
#[ignore = "reads the local session directory; run explicitly with --ignored"]
fn measures_estimator_error_against_real_usage() {
    let root = std::env::var("HF_SESSION_DIR").unwrap_or_else(|_| {
        format!(
            "{}/.heartflow/sessions",
            std::env::var("USERPROFILE").unwrap_or_default()
        )
    });

    let mut raw_predicted = Vec::new();
    let mut calibrated_predicted = Vec::new();
    let mut actuals = Vec::new();
    let mut sessions_used = 0usize;
    let mut densities = Vec::new();
    let mut overheads = Vec::new();

    for entry in fs::read_dir(&root).expect("session dir").flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        // The shipped loader, so the probe parses sessions exactly as the
        // runtime does.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Ok(session) = Session::from_json(&value) else {
            continue;
        };

        // The prompt a turn charged for is the message list as it stood when the
        // request went out: everything before the message that carries the
        // usage.
        let mut samples = Vec::new();
        for (index, message) in session.messages.iter().enumerate() {
            if let Some(usage) = message.usage {
                let predicted = estimate_tokens_from(&session.messages[..index], 0, 0);
                let actual = usage.context_input_tokens();
                if predicted >= MIN_SAMPLE_TOKENS && actual > 0 {
                    samples.push((predicted, actual));
                }
            }
        }
        if samples.len() < 3 {
            continue;
        }
        sessions_used += 1;

        let mut calibration = TokenCalibration::default();
        for (predicted, actual) in &samples {
            // Predict first (learning only from earlier turns), then observe.
            calibrated_predicted.push(calibration.apply(*predicted));
            calibration.observe(*predicted, *actual);
            raw_predicted.push(*predicted);
            actuals.push(*actual);
        }
        densities.push(calibration.density());
        overheads.push(calibration.overhead_tokens());
    }

    if actuals.is_empty() {
        println!("no usable sessions under {root}; nothing to measure");
        return;
    }

    let report = |label: &str, predicted: &[usize]| -> f64 {
        let mut errors: Vec<f64> = predicted
            .iter()
            .zip(&actuals)
            .map(|(p, a)| (*p as f64 - *a as f64).abs() / *a as f64)
            .collect();
        errors.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        let bias: f64 = predicted
            .iter()
            .zip(&actuals)
            .map(|(p, a)| (*p as f64 - *a as f64) / *a as f64)
            .sum::<f64>()
            / actuals.len() as f64;
        let mean = errors.iter().sum::<f64>() / errors.len() as f64;
        println!(
            "{label:<11} median={:.1}% mean={:.1}% p90={:.1}% bias={:.1}%",
            percentile(&errors, 0.5) * 100.0,
            mean * 100.0,
            percentile(&errors, 0.9) * 100.0,
            bias * 100.0
        );
        mean
    };

    println!("sessions={sessions_used} turns={}", actuals.len());
    let raw_mean = report("raw", &raw_predicted);
    let calibrated_mean = report("calibrated", &calibrated_predicted);

    let median = |values: &mut Vec<f64>| {
        values.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        values[values.len() / 2]
    };
    println!(
        "learned density={:.2} overhead={:.0} tokens",
        median(&mut densities),
        median(&mut overheads)
    );

    // The one invariant worth failing on: the correction must beat the
    // uncorrected heuristic on the same data.
    assert!(
        calibrated_mean < raw_mean,
        "calibration made things worse: {calibrated_mean:.3} vs {raw_mean:.3}"
    );
}
