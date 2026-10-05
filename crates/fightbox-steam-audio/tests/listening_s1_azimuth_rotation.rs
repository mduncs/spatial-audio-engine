//! Free-field azimuth-rotation listening stimulus.
//!
//! Ignored because it bakes pathing and renders the pinned firefighter-siren
//! recording through one retained linked-SDK moving-source session.

#[path = "support/listening_scene.rs"]
mod listening_scene;

use listening_scene::{
    BLOCK_FRAMES, SAMPLE_RATE, Source, channel_energy_balance, parse_fixture, peak, render_scene,
    rms, rms_db, stereo_window, write_summed,
};

const MIN_SUSTAINED_BALANCE: f32 = 0.03;

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and pinned siren WAV"]
fn orbiting_siren_sustains_interaural_balance_swings_in_both_directions() {
    let fixture = parse_fixture(include_str!(
        "../../../fixtures/s1-azimuth-rotation/fixture.json"
    ));
    assert_eq!(fixture.id, "s1-free-field-rotation");
    assert_eq!(fixture.gate, "S1");
    assert_eq!(fixture.asset_id, "ff-siren");
    let Source::ClosedCycle(cycle) = &fixture.source else {
        panic!("S1 source must carry a closed orbit");
    };
    assert!((cycle.speed_mps - 3.0).abs() < f32::EPSILON);

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

    let duration_s = cycle.duration_s();
    let north_balance = channel_energy_balance(stereo_window(
        &render.interleaved,
        duration_s * 0.25 - 0.5,
        duration_s * 0.25 + 0.5,
    ));
    let south_balance = channel_energy_balance(stereo_window(
        &render.interleaved,
        duration_s * 0.75 - 0.5,
        duration_s * 0.75 + 0.5,
    ));
    assert!(
        north_balance * south_balance < 0.0,
        "opposite sides of the orbit must favor opposite ears"
    );
    assert!(
        north_balance.abs() >= MIN_SUSTAINED_BALANCE
            && south_balance.abs() >= MIN_SUSTAINED_BALANCE,
        "one-second side windows must sustain measurable balance in both directions"
    );

    let radial_errors = trajectory.iter().map(|sample| {
        let east_m = sample.position.east_m - fixture.listener.pose.position.east_m;
        let north_m = sample.position.north_m - fixture.listener.pose.position.north_m;
        let up_m = sample.position.up_m - fixture.listener.pose.position.up_m;
        let radius = (east_m.powi(2) + north_m.powi(2) + up_m.powi(2)).sqrt();
        (radius - 12.0).abs()
    });
    let max_radial_error_m = radial_errors.fold(0.0_f32, f32::max);
    assert!(max_radial_error_m < 0.03);

    println!(
        "S1 orbit distance={:.3}m speed={:.3}m/s duration={:.3}s blocks={} max_radial_error={max_radial_error_m:.6}m peak={output_peak:.8e} rms={output_rms:.8e} rms_dbfs={:.3}",
        cycle.total_distance_m(),
        cycle.speed_mps,
        duration_frames as f32 / SAMPLE_RATE as f32,
        trajectory.len(),
        rms_db(&render.interleaved),
    );
    println!(
        "S1 sustained balance north={north_balance:.4} south={south_balance:.4} magnitudes>{MIN_SUSTAINED_BALANCE:.4}"
    );

    let wav_path = write_summed(
        "s1-azimuth-rotation",
        "s1-azimuth-rotation-summed.wav",
        &render.interleaved,
    );
    println!("S1 WAV output={}", wav_path.display());
}
