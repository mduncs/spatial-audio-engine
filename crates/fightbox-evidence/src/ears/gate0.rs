use super::corpus::{CORRUPTIONS, Corruption, clean, corrupted};
use super::{ExtractorMetrics, analyze};

const MIN_MARGIN: f64 = 1.0e-6;

#[derive(Debug)]
struct Discrimination {
    metric: &'static str,
    clean: f64,
    corrupt: f64,
    margin: f64,
    required_margin: f64,
}

fn discrimination(
    kind: Corruption,
    clean: ExtractorMetrics,
    corrupt: ExtractorMetrics,
) -> Discrimination {
    let (metric, clean_value, corrupt_value, required_margin) = match kind {
        // Fixed 30 ms copy must lift long-lag correlation by at least 0.15.
        Corruption::SlapbackEcho => (
            "slapback_correlation",
            clean.slapback_correlation,
            corrupt.slapback_correlation,
            0.15,
        ),
        // Fixed 2 ms copy must lift short-lag correlation by at least 0.10.
        Corruption::CoherentComb => (
            "comb_correlation",
            clean.comb_correlation,
            corrupt.comb_correlation,
            0.10,
        ),
        // A stepped gain trajectory must add at least 3 dB of robust envelope jump.
        Corruption::Zipper => (
            "zipper_step_db",
            clean.zipper_step_db,
            corrupt.zipper_step_db,
            3.0,
        ),
        // Clean cues agree; the corrupted HRTF must conflict in at least 80% more frames.
        Corruption::FlippedHrtf => (
            "cue_conflict_fraction",
            clean.cue_conflict_fraction,
            corrupt.cue_conflict_fraction,
            0.80,
        ),
        // Volume-only occlusion must lose at least 8 dB while retaining spectral shape.
        Corruption::OcclusionAsVolume => (
            "level_loss_db",
            0.0,
            clean.loudness_dbfs - corrupt.loudness_dbfs,
            8.0,
        ),
        // Splice impulses must add at least 10 robust derivative sigma.
        Corruption::SpliceClicks => (
            "click_derivative_z",
            clean.click_derivative_z,
            corrupt.click_derivative_z,
            10.0,
        ),
        // The pitch discontinuity must exceed clean's largest step by 250 cents.
        Corruption::SteppedPitch => (
            "pitch_step_cents",
            clean.pitch_step_cents,
            corrupt.pitch_step_cents,
            250.0,
        ),
        // Mono collapse must lift peak IACC by at least 0.02.
        Corruption::MonoCollapsed => ("iacc", clean.iacc, corrupt.iacc, 0.02),
        // Abrupt reverb-character replacement must add 0.10 profile L1 distance.
        Corruption::SteppedEnclosure => (
            "enclosure_step",
            clean.enclosure_step,
            corrupt.enclosure_step,
            0.10,
        ),
        // A 3 Hz limiter oscillation must add 0.20 normalized modulation.
        Corruption::PumpingLimiter => (
            "pump_modulation",
            clean.pump_modulation,
            corrupt.pump_modulation,
            0.20,
        ),
        // Hard cull must create at least 25% terminal digital silence.
        Corruption::AbruptCull => (
            "trailing_silence_fraction",
            clean.trailing_silence_fraction,
            corrupt.trailing_silence_fraction,
            0.25,
        ),
    };
    Discrimination {
        metric,
        clean: clean_value,
        corrupt: corrupt_value,
        margin: corrupt_value - clean_value,
        required_margin,
    }
}

fn assert_clean_passes(metrics: ExtractorMetrics) {
    let scalar_outputs = [
        metrics.slapback_correlation,
        metrics.comb_correlation,
        metrics.zipper_step_db,
        metrics.itd_ms,
        metrics.ild_db,
        metrics.cue_conflict_fraction,
        metrics.iacc,
        metrics.width,
        metrics.loudness_dbfs,
        metrics.spectral_tilt_db,
        metrics.spectral_flux,
        metrics.click_derivative_z,
        metrics.pitch_step_cents,
        metrics.pump_modulation,
        metrics.enclosure_step,
        metrics.trailing_silence_fraction,
        metrics.reflection_density,
    ];
    assert!(scalar_outputs.iter().all(|value| value.is_finite()));
    assert!(
        metrics
            .coherence_spectrum
            .iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(value))
    );
    assert!(metrics.slapback_correlation < 0.90);
    assert!(metrics.comb_correlation < 0.90);
    assert!(metrics.zipper_step_db < 3.0);
    assert!(metrics.cue_conflict_fraction < 0.05);
    assert!(metrics.iacc < 0.99);
    assert!(metrics.spectral_flux < 0.25);
    assert!(metrics.click_derivative_z < 15.0);
    assert!(metrics.pitch_step_cents < 100.0);
    assert!(metrics.pump_modulation < 0.15);
    assert!(metrics.enclosure_step < 0.10);
    assert!(metrics.trailing_silence_fraction < MIN_MARGIN);
    assert!((metrics.reflection_density - 1.0).abs() < 0.25);
}

#[test]
fn gate0_corruption_corpus_discriminates_every_class_and_clean_passes() {
    let clean_metrics = analyze(clean().pcm()).expect("clean corpus must analyze");
    eprintln!("gate0 clean metrics: {clean_metrics:#?}");
    assert_clean_passes(clean_metrics);

    let mut failures = Vec::new();
    for kind in CORRUPTIONS {
        let corrupt_metrics =
            analyze(corrupted(kind).pcm()).expect("corruption corpus must analyze");
        let result = discrimination(kind, clean_metrics, corrupt_metrics);
        eprintln!(
            "gate0 {kind:?}: {} clean={:.6} corrupt={:.6} margin={:.6} required={:.6}",
            result.metric, result.clean, result.corrupt, result.margin, result.required_margin
        );
        if result.margin < result.required_margin {
            failures.push(format!(
                "{kind:?}: {} margin {:.6} < {:.6}",
                result.metric, result.margin, result.required_margin
            ));
        }
        if kind == Corruption::OcclusionAsVolume {
            let tilt_change =
                (corrupt_metrics.spectral_tilt_db - clean_metrics.spectral_tilt_db).abs();
            if tilt_change > 0.05 {
                failures.push(format!(
                    "{kind:?}: spectral shape changed by {tilt_change:.6} dB"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "Gate 0 left corruption classes undiscriminated:\n{}",
        failures.join("\n")
    );
}
