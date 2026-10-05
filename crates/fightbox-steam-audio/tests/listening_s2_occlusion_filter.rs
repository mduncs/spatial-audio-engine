//! Masonry-wall occlusion-filter listening stimulus.
//!
//! Ignored because it bakes pathing and renders the pinned firefighter-siren
//! recording through one retained linked-SDK moving-source session.

#[path = "support/listening_scene.rs"]
mod listening_scene;

use listening_scene::{
    BLOCK_FRAMES, SAMPLE_RATE, Source, high_pass_rms, mono_window, parse_fixture, peak,
    render_scene, rms, rms_db, stereo_window, write_summed,
};

const HIGH_PASS_HZ: f32 = 8_000.0;
const MIN_LEVEL_DROP_DB: f32 = 1.0;
const MIN_HIGH_FREQUENCY_LOSS_DB: f32 = 0.25;

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and pinned siren WAV"]
fn wall_crossings_reduce_level_and_high_frequency_energy_when_occluded() {
    let fixture = parse_fixture(include_str!(
        "../../../fixtures/s2-occlusion-filter/fixture.json"
    ));
    assert_eq!(fixture.id, "s2-direct-occlusion");
    assert_eq!(fixture.gate, "S2");
    assert_eq!(fixture.asset_id, "ff-siren");
    let Source::ClosedCycle(cycle) = &fixture.source else {
        panic!("S2 source must carry a closed wall-crossing trajectory");
    };
    assert!((cycle.speed_mps - 1.6).abs() < f32::EPSILON);
    assert!((cycle.total_distance_m() - 40.0).abs() < 1.0e-4);

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

    let leg_s = 10.0 / cycle.speed_mps;
    let los_centers_s = [0.9, 2.0 * leg_s - 0.9, 2.0 * leg_s + 0.9, 4.0 * leg_s - 0.9];
    let occluded_centers_s = [
        leg_s - 0.9,
        leg_s + 0.9,
        3.0 * leg_s - 0.9,
        3.0 * leg_s + 0.9,
    ];
    let transfer_metrics = |center_s: f32| {
        let output = stereo_window(&render.interleaved, center_s - 0.4, center_s + 0.4);
        let input = mono_window(&render.input_mono, center_s - 0.4, center_s + 0.4);
        let level_transfer_db = rms_db(output) - rms_db(input);
        let output_high_fraction_db =
            rms_db_value(high_pass_rms(output, 2, HIGH_PASS_HZ)) - rms_db(output);
        let input_high_fraction_db =
            rms_db_value(high_pass_rms(input, 1, HIGH_PASS_HZ)) - rms_db(input);
        (
            level_transfer_db,
            output_high_fraction_db - input_high_fraction_db,
        )
    };
    let average_metrics = |centers: &[f32]| {
        let (level_sum, high_sum) = centers
            .iter()
            .copied()
            .map(&transfer_metrics)
            .fold((0.0_f32, 0.0_f32), |(level_sum, high_sum), metrics| {
                (level_sum + metrics.0, high_sum + metrics.1)
            });
        (
            level_sum / centers.len() as f32,
            high_sum / centers.len() as f32,
        )
    };
    let (los_level_transfer_db, los_high_transfer_db) = average_metrics(&los_centers_s);
    let (occluded_level_transfer_db, occluded_high_transfer_db) =
        average_metrics(&occluded_centers_s);
    let level_drop_db = los_level_transfer_db - occluded_level_transfer_db;
    let high_frequency_loss_db = los_high_transfer_db - occluded_high_transfer_db;

    println!(
        "S2 distance={:.3}m speed={:.3}m/s duration={:.3}s blocks={} peak={output_peak:.8e} rms={output_rms:.8e} rms_dbfs={:.3}",
        cycle.total_distance_m(),
        cycle.speed_mps,
        duration_frames as f32 / SAMPLE_RATE as f32,
        trajectory.len(),
        rms_db(&render.interleaved),
    );
    println!(
        "S2 transfer LOS level={los_level_transfer_db:.3}dB high={los_high_transfer_db:.3}dB occluded level={occluded_level_transfer_db:.3}dB high={occluded_high_transfer_db:.3}dB level_drop={level_drop_db:.3}>{MIN_LEVEL_DROP_DB:.3}dB high_frequency_loss={high_frequency_loss_db:.3}>{MIN_HIGH_FREQUENCY_LOSS_DB:.3}dB cutoff={HIGH_PASS_HZ:.0}Hz"
    );
    assert!(
        level_drop_db >= MIN_LEVEL_DROP_DB,
        "occluded windows must show a generous minimum level drop versus LOS"
    );
    assert!(
        high_frequency_loss_db >= MIN_HIGH_FREQUENCY_LOSS_DB,
        "occluded windows must show a generous minimum high-frequency loss versus LOS"
    );

    let wav_path = write_summed(
        "s2-occlusion-filter",
        "s2-occlusion-filter-summed.wav",
        &render.interleaved,
    );
    println!("S2 WAV output={}", wav_path.display());
}

fn rms_db_value(value: f32) -> f32 {
    20.0 * value.max(f32::MIN_POSITIVE).log10()
}
