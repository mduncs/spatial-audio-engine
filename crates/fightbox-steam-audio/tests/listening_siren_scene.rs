//! Bounded moving-siren listening stimulus.
//!
//! Ignored because it bakes pathing and renders the pinned firefighter-siren
//! recording through one retained linked-SDK multi-source session.

#[path = "support/listening_scene.rs"]
mod listening_scene;

use listening_scene::{
    BLOCK_FRAMES, SAMPLE_RATE, Source, channel_energy_balance, mono_window, parse_fixture, peak,
    render_scene, rms, rms_db, stereo_window, write_summed,
};

const MIN_NEAR_OVER_FAR_DB: f32 = 3.0;
const MIN_BALANCE_CHANGE: f32 = 0.03;

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and pinned siren WAV"]
fn bounded_siren_cycle_exercises_approach_recede_and_canyon_returns() {
    let fixture = parse_fixture(include_str!("../../../fixtures/s-siren/fixture.json"));
    assert_eq!(fixture.id, "s-siren-bounded-masonry-canyon");
    assert_eq!(fixture.asset_id, "ff-siren");
    let Source::ClosedCycle(cycle) = &fixture.source else {
        panic!("siren source must carry a closed trajectory");
    };
    assert!((cycle.speed_mps - 8.0).abs() < f32::EPSILON);
    assert!((cycle.total_distance_m() - 82.0).abs() < 1.0e-4);
    let trajectory = cycle.block_samples();
    let duration_frames = trajectory.len() * BLOCK_FRAMES;
    let render = render_scene(&fixture, &trajectory, duration_frames);
    assert_eq!(render.interleaved.len(), duration_frames * 2);
    assert!(
        render.interleaved.iter().all(|sample| sample.is_finite()),
        "full pipeline must stay finite; pathing is intentionally initialized and retained"
    );
    let output_peak = peak(&render.interleaved);
    let output_rms = rms(&render.interleaved);
    assert!(output_peak > 1.0e-8 && output_rms > 1.0e-9);

    // The first 4.5-second leg passes the listener. Normalize each output
    // window by its local source-program RMS so siren wail dynamics do not
    // masquerade as distance motion.
    let approach_transfer_db = rms_db(stereo_window(&render.interleaved, 0.75, 1.25))
        - rms_db(mono_window(&render.input_mono, 0.75, 1.25));
    let near_transfer_db = rms_db(stereo_window(&render.interleaved, 2.0, 2.5))
        - rms_db(mono_window(&render.input_mono, 2.0, 2.5));
    let recede_transfer_db = rms_db(stereo_window(&render.interleaved, 3.25, 3.75))
        - rms_db(mono_window(&render.input_mono, 3.25, 3.75));
    let far_transfer_db = 0.5 * (approach_transfer_db + recede_transfer_db);
    let near_over_far_db = near_transfer_db - far_transfer_db;

    let before_balance = channel_energy_balance(stereo_window(&render.interleaved, 1.75, 2.05));
    let after_balance = channel_energy_balance(stereo_window(&render.interleaved, 2.45, 2.75));
    let balance_change = (after_balance - before_balance).abs();

    let first = &trajectory[0].position;
    let last = &trajectory[trajectory.len() - 1].position;
    let closure_error_m = ((last.east_m - first.east_m).powi(2)
        + (last.north_m - first.north_m).powi(2)
        + (last.up_m - first.up_m).powi(2))
    .sqrt();
    println!(
        "siren distance={:.3}m speed={:.3}m/s duration={:.3}s blocks={} closure_error={closure_error_m:.6}m peak={output_peak:.8e} rms={output_rms:.8e} rms_dbfs={:.3}",
        cycle.total_distance_m(),
        cycle.speed_mps,
        duration_frames as f32 / SAMPLE_RATE as f32,
        trajectory.len(),
        rms_db(&render.interleaved),
    );
    println!(
        "siren transfer approach={approach_transfer_db:.3}dB near={near_transfer_db:.3}dB recede={recede_transfer_db:.3}dB near_over_far={near_over_far_db:.3}>{MIN_NEAR_OVER_FAR_DB:.3}dB balance_before={before_balance:.4} after={after_balance:.4} change={balance_change:.4}>{MIN_BALANCE_CHANGE:.4}"
    );
    assert!(closure_error_m < 1.0e-4);
    assert!(
        near_over_far_db >= MIN_NEAR_OVER_FAR_DB,
        "listener pass must produce a modest near-over-far level rise"
    );
    assert!(
        balance_change >= MIN_BALANCE_CHANGE,
        "cross-listener motion must measurably change binaural balance"
    );

    let wav_path = write_summed("s-siren", "siren-summed.wav", &render.interleaved);
    println!("siren WAV output={}", wav_path.display());
}
