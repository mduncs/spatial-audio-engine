//! Distant static-bell listening stimulus.
//!
//! Ignored because it bakes pathing and renders the pinned church-bell
//! recording through one retained linked-SDK multi-source session.

#[path = "support/listening_scene.rs"]
mod listening_scene;

use fightbox_api::EnuVector3;
use listening_scene::{
    SAMPLE_RATE, Source, TrajectorySample, parse_fixture, peak, render_scene, rms, rms_db,
    write_summed,
};

const DURATION_SECONDS: usize = 20;
const MIN_CREST_FACTOR: f32 = 2.0;
const MAX_OUTPUT_TO_INPUT_DB: f32 = -20.0;

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and pinned church-bells WAV"]
fn distant_bell_retains_stable_level_and_strike_shape_in_reflective_scene() {
    let fixture = parse_fixture(include_str!("../../../fixtures/s-bell/fixture.json"));
    assert_eq!(fixture.id, "s-bell-distant-reflective-canyon");
    assert_eq!(fixture.asset_id, "church-bells");
    let Source::Static(position) = fixture.source else {
        panic!("bell source must be static");
    };
    let distance_m = fixture.source_distance_m();
    assert!((200.0..=300.0).contains(&distance_m));

    let trajectory = [TrajectorySample {
        position,
        velocity_mps: EnuVector3::default(),
    }];
    let duration_frames = DURATION_SECONDS * SAMPLE_RATE as usize;
    let render = render_scene(&fixture, &trajectory, duration_frames);
    assert!(
        render.interleaved.iter().all(|sample| sample.is_finite()),
        "full pipeline must stay finite; pathing is intentionally initialized and retained"
    );
    let output_peak = peak(&render.interleaved);
    let output_rms = rms(&render.interleaved);
    let input_rms = rms(&render.input_mono);
    assert!(output_peak > 1.0e-9 && output_rms > 1.0e-10);
    let crest_factor = output_peak / output_rms;
    let output_to_input_db = 20.0 * (output_rms / input_rms).log10();
    println!(
        "bell distance={distance_m:.3}m duration={DURATION_SECONDS}s peak={output_peak:.8e} rms={output_rms:.8e} rms_dbfs={:.3} crest={crest_factor:.3}>{MIN_CREST_FACTOR:.3} input_rms={input_rms:.8e} output/input={output_to_input_db:.3}<{MAX_OUTPUT_TO_INPUT_DB:.3}dB",
        rms_db(&render.interleaved),
    );
    assert!(
        crest_factor >= MIN_CREST_FACTOR,
        "distant summed bell must retain modest strike-to-program contrast"
    );
    assert!(
        output_to_input_db <= MAX_OUTPUT_TO_INPUT_DB,
        "few-hundred-metre propagation must remain materially below source level"
    );

    let wav_path = write_summed("s-bell", "bell-summed.wav", &render.interleaved);
    println!("bell WAV output={}", wav_path.display());
}
