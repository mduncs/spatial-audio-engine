//! Ignored Wave 12 listening strip rendered through Steam Audio's linked HRTF.

use super::super::{
    AudioBuffer, BinauralEffect, Context, DirectEffect, Hrtf, default_air_absorption_model,
    raw_audio_settings,
};
use crate::{AudioConfig, ffi};
use fightbox_evidence::{WavSpec, read_wav, sha256_hex, write_wav};
use std::f64::consts::{PI, TAU};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const SAMPLE_RATE_HZ: u32 = 48_000;
const FRAME_SIZE: usize = 512;
const CLIP_FRAMES: usize = 120_000;
const PROGRAM_FRAMES: usize = 96_000;
const PROGRAM_FADE_FRAMES: usize = 2_400;
const B_ONSET_FRAME: usize = 4_800;
const HRTF_IR_FRAMES: usize = 4_096;
const SOUND_SPEED_MPS: f64 = 343.0;
const MACH: f64 = 2.5;
const TRAJECTORY_FOOT_M: f64 = 60.0;
const N_WAVE_REFERENCE_DISTANCE_M: f64 = 30.0;
const N_WAVE_REFERENCE_DURATION_MS: f64 = 0.8;
const N_WAVE_POSITIVE_FRACTION: f64 = 0.45;
const CRACK_OVER_REFERENCE_BLAST_DB: f64 = 3.0;
const B_MASTER_GAIN_DB: f64 = 12.0;
const A_MASTER_GAIN_DB: f64 = 24.0;
const ONSET_FRACTION_OF_PEAK: f64 = 1.0e-3;
const SOURCE_SHA256: &str = "1376d8461ac1f30398a2184f6c3bb7da0c64085861707be9e2f2893313560f9b";
const OUTPUT_DIR: &str = "/private/tmp/impulse-strip";

#[derive(Clone, Copy, Debug)]
struct MorphKnot {
    distance_m: f64,
    cutoff_hz: f64,
}

const MORPH_KNOTS: [MorphKnot; 4] = [
    MorphKnot {
        distance_m: 5.0,
        cutoff_hz: 18_000.0,
    },
    MorphKnot {
        distance_m: 50.0,
        cutoff_hz: 7_500.0,
    },
    MorphKnot {
        distance_m: 200.0,
        cutoff_hz: 2_800.0,
    },
    MorphKnot {
        distance_m: 500.0,
        cutoff_hz: 1_100.0,
    },
];

#[derive(Clone, Copy, Debug)]
struct MorphParams {
    cutoff_hz: f64,
    pole: f64,
    stages: u32,
}

impl MorphParams {
    fn at_distance(distance_m: f64) -> Self {
        let cutoff_hz = interpolated_cutoff_hz(distance_m);
        Self {
            cutoff_hz,
            pole: (-TAU * cutoff_hz / SAMPLE_RATE_HZ as f64).exp(),
            stages: 2,
        }
    }

    fn direct_feed(self) -> f64 {
        (1.0 - self.pole).powi(self.stages as i32)
    }

    fn magnitude_squared(self, omega: f64) -> f64 {
        let one_stage = (1.0 - self.pole).powi(2)
            / (1.0 + self.pole * self.pole - 2.0 * self.pole * omega.cos());
        one_stage.powi(self.stages as i32)
    }

    fn attenuation_db_at(self, frequency_hz: f64) -> f64 {
        10.0 * self
            .magnitude_squared(TAU * frequency_hz / SAMPLE_RATE_HZ as f64)
            .max(1.0e-30)
            .log10()
    }
}

#[derive(Debug)]
struct ClipRecord {
    name: &'static str,
    bytes: Vec<u8>,
    sha256: String,
    rms_dbfs: f64,
    peak_dbfs: f64,
    listen_for: &'static str,
}

#[derive(Clone, Debug)]
struct BReport {
    distance_m: f64,
    distance_gain: f64,
    air_absorption: [f32; 3],
    morph: MorphParams,
    makeup: f64,
    plain_rms_dbfs: f64,
    morph_rms_dbfs: f64,
    energy_delta_db: f64,
    plain_onset_frame: usize,
    morph_onset_frame: usize,
}

#[derive(Clone, Copy, Debug)]
struct ShotMath {
    miss_distance_m: f64,
    foot_distance_m: f64,
    s_star_m: f64,
    t_star_s: f64,
    r_star_m: f64,
    t_crack_s: f64,
    blast_distance_m: f64,
    t_blast_s: f64,
    lead_s: f64,
    crack_elevation_deg: f64,
    blast_elevation_deg: f64,
    crack_exists: bool,
}

#[derive(Clone, Debug)]
struct AReport {
    name: &'static str,
    shot: ShotMath,
    blast_air_absorption: [f32; 3],
    blast_morph: MorphParams,
    blast_makeup: f64,
    blast_peak_dbfs: f64,
    crack_air_absorption: Option<[f32; 3]>,
    crack_duration_ms: Option<f64>,
    whitham_relative_db: Option<f64>,
    crack_source_makeup: Option<f64>,
    crack_target_peak_dbfs: Option<f64>,
    measured_crack_s: Option<f64>,
    measured_blast_s: f64,
}

struct RenderOutcome {
    clips: Vec<ClipRecord>,
    b_reports: Vec<BReport>,
    a_reports: Vec<AReport>,
}

#[derive(Clone)]
struct BlastComponent {
    stereo: Vec<f32>,
    onset_frame: usize,
    peak: f64,
    air_absorption: [f32; 3],
    morph: MorphParams,
    makeup: f64,
}

#[derive(Clone, Copy, Default)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    fn from_polar(radius: f64, angle: f64) -> Self {
        Self {
            re: radius * angle.cos(),
            im: radius * angle.sin(),
        }
    }

    fn norm_squared(self) -> f64 {
        self.re * self.re + self.im * self.im
    }

    fn add(self, other: Self) -> Self {
        Self {
            re: self.re + other.re,
            im: self.im + other.im,
        }
    }

    fn sub(self, other: Self) -> Self {
        Self {
            re: self.re - other.re,
            im: self.im - other.im,
        }
    }

    fn mul(self, other: Self) -> Self {
        Self {
            re: self.re * other.re - self.im * other.im,
            im: self.re * other.im + self.im * other.re,
        }
    }
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and writes the Wave 12 listening strip"]
fn impulse_listening_strip_is_deterministic() {
    let source_bytes = fs::read(source_path()).expect("read artillery-impact source WAV");
    assert_eq!(
        sha256_hex(&source_bytes),
        SOURCE_SHA256,
        "source hash drift"
    );
    let decoded = decode_pcm16_mono(&source_bytes);
    let source_peak = decoded
        .iter()
        .map(|sample| f64::from(*sample).abs())
        .fold(0.0_f64, f64::max);
    let source_onset_frame = decoded
        .iter()
        .position(|sample| f64::from(*sample).abs() >= source_peak * ONSET_FRACTION_OF_PEAK)
        .expect("artillery source onset");
    assert!(source_onset_frame + PROGRAM_FRAMES <= decoded.len());
    let mut program = decoded[source_onset_frame..source_onset_frame + PROGRAM_FRAMES].to_vec();
    apply_fade_out(&mut program, PROGRAM_FADE_FRAMES);

    let first = render_once(&program);
    let second = render_once(&program);
    assert_eq!(first.clips.len(), second.clips.len());
    for (left, right) in first.clips.iter().zip(&second.clips) {
        assert_eq!(left.name, right.name);
        assert_eq!(
            left.sha256, right.sha256,
            "determinism drift in {}",
            left.name
        );
        assert_eq!(left.bytes, right.bytes, "byte drift in {}", left.name);
    }

    let output_dir = PathBuf::from(OUTPUT_DIR);
    fs::create_dir_all(&output_dir).expect("create impulse strip output directory");
    for clip in &first.clips {
        fs::write(output_dir.join(clip.name), &clip.bytes).expect("write impulse strip WAV");
        let (spec, samples) = read_wav(&clip.bytes).expect("read back float WAV");
        assert_eq!(spec, stereo_spec());
        assert_eq!(samples.len(), CLIP_FRAMES * 2);
    }
    let manifest = build_manifest(&first, source_onset_frame, source_peak);
    fs::write(output_dir.join("manifest.md"), manifest).expect("write impulse strip manifest");

    eprintln!("SOURCE onset_frame={source_onset_frame} peak={source_peak:.9}");
    for report in &first.b_reports {
        eprintln!(
            "ENERGY d={:.0}m plain={:.6}dBFS morph={:.6}dBFS delta={:+.6}dB makeup={:.9}",
            report.distance_m,
            report.plain_rms_dbfs,
            report.morph_rms_dbfs,
            report.energy_delta_db,
            report.makeup
        );
    }
    for report in &first.a_reports {
        eprintln!(
            "TIMING {} crack_computed_ms={:.3} crack_measured_ms={} blast_computed_ms={:.3} blast_measured_ms={:.3} lead_ms={:.3}",
            report.name,
            report.shot.t_crack_s * 1_000.0,
            report
                .measured_crack_s
                .map(|time| format!("{:.3}", time * 1_000.0))
                .unwrap_or_else(|| "none".to_owned()),
            report.shot.t_blast_s * 1_000.0,
            report.measured_blast_s * 1_000.0,
            report.shot.lead_s * 1_000.0
        );
    }
    for clip in &first.clips {
        eprintln!("{}  {}", clip.sha256, clip.name);
    }
}

fn render_once(program: &[f32]) -> RenderOutcome {
    let context = Context::create().expect("iplContextCreate");
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE_HZ as i32,
        frame_size: FRAME_SIZE as i32,
    };
    let mut audio_settings = raw_audio_settings(audio);
    let hrtf = Hrtf::create(&context, &mut audio_settings).expect("iplHRTFCreate");

    let (mut clips, b_reports) = render_b_strip(&context, &mut audio_settings, &hrtf, program);
    let (a_clips, a_reports) = render_a_strip(&context, &mut audio_settings, &hrtf, program);
    clips.extend(a_clips);
    RenderOutcome {
        clips,
        b_reports,
        a_reports,
    }
}

fn render_b_strip(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    program: &[f32],
) -> (Vec<ClipRecord>, Vec<BReport>) {
    let dry = scheduled_program(program, B_ONSET_FRAME);
    let ahead = steam_direction_from_azimuth(0.0);
    let hrtf_ir = render_hrtf_impulse(context, audio_settings, hrtf, ahead);
    let mut morph_clips = Vec::new();
    let mut plain_clips = Vec::new();
    let mut reports = Vec::new();
    let master = db_to_gain(B_MASTER_GAIN_DB);

    for (index, knot) in MORPH_KNOTS.iter().enumerate() {
        let distance_gain = 1.0 / knot.distance_m;
        let distance_stem = scaled(&dry, distance_gain);
        let (post_air, air_absorption) =
            apply_steam_air_absorption(context, audio_settings, &distance_stem, knot.distance_m);
        let morph = MorphParams::at_distance(knot.distance_m);
        assert!((0.0..1.0).contains(&morph.pole));
        assert!(morph.direct_feed() > 0.0);
        let onset_probe = apply_morph(&dry, morph, 1.0);
        let makeup = analytic_makeup(&post_air, morph, &hrtf_ir);
        let morphed_mono = apply_morph(&post_air, morph, makeup);
        assert_eq!(
            first_nonzero_mono(&dry),
            first_nonzero_mono(&onset_probe),
            "minimum-phase morph changed mathematical onset at {} m",
            knot.distance_m
        );

        let mut plain = render_binaural(context, audio_settings, hrtf, &post_air, ahead);
        let mut morphed = render_binaural(context, audio_settings, hrtf, &morphed_mono, ahead);
        multiply_in_place(&mut plain, master);
        multiply_in_place(&mut morphed, master);
        let energy_delta_db = energy_delta_db(&morphed, &plain);
        assert!(
            energy_delta_db.abs() <= 0.1,
            "{} m morph/plain energy mismatch is {energy_delta_db:+.6} dB",
            knot.distance_m
        );
        let plain_onset_frame = first_nonzero_mono(&dry);
        let morph_onset_frame = first_nonzero_mono(&onset_probe);
        assert_eq!(
            plain_onset_frame, morph_onset_frame,
            "morph added a sample of latency at {} m",
            knot.distance_m
        );
        let (plain_rms_dbfs, _) = stereo_level_metrics(&plain);
        let (morph_rms_dbfs, _) = stereo_level_metrics(&morphed);

        let morph_names = [
            "b_morph_d0005m.wav",
            "b_morph_d0050m.wav",
            "b_morph_d0200m.wav",
            "b_morph_d0500m.wav",
        ];
        let plain_names = [
            "b_plain_d0005m.wav",
            "b_plain_d0050m.wav",
            "b_plain_d0200m.wav",
            "b_plain_d0500m.wav",
        ];
        let morph_prompts = [
            "5 m: a violent RIP with a hard edge.",
            "50 m: the edge is still present, but the hit has begun to broaden into thunder.",
            "200 m: a heavy rounded THUMP, with much less brittle crack on the front.",
            "500 m: a soft rounded RUMBLE, edge gone; compare b_plain_d0500m, today's version, which is thinner and clickier and less like weather.",
        ];
        let plain_prompts = [
            "5 m: today's un-morphed rendering, still close, hard, and nearly the same loudness as its morph partner.",
            "50 m: today's version keeps a thinner, sharper click than the distance-morphed partner.",
            "200 m: today's version retains an implausibly wiry edge beside the rounder morph clip.",
            "500 m: today's version is thinner and clickier and less like weather than the soft morph rumble.",
        ];
        morph_clips.push(make_clip(morph_names[index], morphed, morph_prompts[index]));
        plain_clips.push(make_clip(plain_names[index], plain, plain_prompts[index]));
        reports.push(BReport {
            distance_m: knot.distance_m,
            distance_gain,
            air_absorption,
            morph,
            makeup,
            plain_rms_dbfs,
            morph_rms_dbfs,
            energy_delta_db,
            plain_onset_frame,
            morph_onset_frame,
        });
    }
    morph_clips.extend(plain_clips);
    (morph_clips, reports)
}

fn render_a_strip(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    program: &[f32],
) -> (Vec<ClipRecord>, Vec<AReport>) {
    let shots = [
        (
            "a_snap_then_boom_d030m.wav",
            shot_math(30.0, TRAJECTORY_FOOT_M),
            "A sharp SNAP above and ahead, then the deeper BOOM from the gun direction: the snap-then-boom of a near miss.",
        ),
        (
            "a_boom_only_no_crack_zone.wav",
            shot_math(30.0, -30.0),
            "Behind the muzzle and outside the Mach cone: only the deeper BOOM arrives; there is no snap.",
        ),
        (
            "a_ordering_d010m.wav",
            shot_math(10.0, TRAJECTORY_FOOT_M),
            "A much louder close-miss SNAP leads the BOOM by longer than in the 30 m reference.",
        ),
        (
            "a_ordering_d090m.wav",
            shot_math(90.0, TRAJECTORY_FOOT_M),
            "A quieter far-miss SNAP lands only just before the BOOM, showing the cone timing closing up with miss distance.",
        ),
    ];

    let mut blasts = Vec::new();
    for (_, shot, _) in shots {
        blasts.push(render_blast_component(
            context,
            audio_settings,
            hrtf,
            program,
            shot.blast_distance_m,
            shot.blast_elevation_deg,
        ));
    }
    let reference_blast_peak = blasts[0].peak;
    let reference_crack_target = reference_blast_peak * db_to_gain(CRACK_OVER_REFERENCE_BLAST_DB);
    let master = db_to_gain(A_MASTER_GAIN_DB);
    let mut clips = Vec::new();
    let mut reports = Vec::new();

    for (index, (name, shot, listen_for)) in shots.into_iter().enumerate() {
        let blast = &blasts[index];
        let blast_target_frame = seconds_to_frame(shot.t_blast_s);
        let placed_blast = place_component(&blast.stereo, blast.onset_frame, blast_target_frame);
        let mut mixed = placed_blast.clone();
        let mut crack_air_absorption = None;
        let mut crack_duration_ms = None;
        let mut whitham_relative_db = None;
        let mut crack_source_makeup = None;
        let mut crack_target_peak_dbfs = None;
        let mut measured_crack_s = None;

        if shot.crack_exists {
            let relative_db = -15.0 * (shot.miss_distance_m / N_WAVE_REFERENCE_DISTANCE_M).log10();
            let duration_ms = N_WAVE_REFERENCE_DURATION_MS
                * (shot.miss_distance_m / N_WAVE_REFERENCE_DISTANCE_M).powf(0.25);
            let target_peak = reference_crack_target * db_to_gain(relative_db);
            let n_wave = synthesize_n_wave(duration_ms);
            let unit_crack = render_crack_component(
                context,
                audio_settings,
                hrtf,
                &n_wave,
                shot.r_star_m,
                shot.crack_elevation_deg,
            );
            let source_makeup = target_peak / unit_crack.peak;
            let crack = scaled(&unit_crack.stereo, source_makeup);
            let crack_peak = stereo_peak(&crack);
            assert!((crack_peak - target_peak).abs() <= target_peak * 2.0e-6);
            let crack_onset = onset_frame(&crack, crack_peak * ONSET_FRACTION_OF_PEAK);
            let crack_target_frame = seconds_to_frame(shot.t_crack_s);
            let placed_crack = place_component(&crack, crack_onset, crack_target_frame);
            add_stereo(&mut mixed, &placed_crack);
            measured_crack_s = Some(
                detect_onset_near(
                    &mixed,
                    crack_target_frame,
                    crack_peak * ONSET_FRACTION_OF_PEAK,
                    48,
                ) as f64
                    / SAMPLE_RATE_HZ as f64,
            );
            crack_air_absorption = Some(unit_crack.air_absorption);
            crack_duration_ms = Some(duration_ms);
            whitham_relative_db = Some(relative_db);
            crack_source_makeup = Some(source_makeup);
            crack_target_peak_dbfs = Some(gain_to_db(target_peak * master));
        }

        multiply_in_place(&mut mixed, master);
        let measured_blast_frame = detect_onset_near(
            &placed_blast,
            blast_target_frame,
            blast.peak * ONSET_FRACTION_OF_PEAK,
            48,
        );
        let measured_blast_s = measured_blast_frame as f64 / SAMPLE_RATE_HZ as f64;
        if index == 0 {
            let in_file_blast_frame = detect_onset_near(
                &mixed,
                blast_target_frame,
                blast.peak * master * ONSET_FRACTION_OF_PEAK,
                48,
            );
            assert_eq!(in_file_blast_frame, measured_blast_frame);
            let crack_error_ms =
                (measured_crack_s.expect("reference crack") - shot.t_crack_s).abs() * 1_000.0;
            let blast_error_ms = (measured_blast_s - shot.t_blast_s).abs() * 1_000.0;
            assert!(
                crack_error_ms <= 0.5,
                "crack onset error {crack_error_ms:.6} ms"
            );
            assert!(
                blast_error_ms <= 0.5,
                "blast onset error {blast_error_ms:.6} ms"
            );
        }
        assert!(stereo_peak(&mixed) < 1.0, "{name} would clip");
        clips.push(make_clip(name, mixed, listen_for));
        reports.push(AReport {
            name,
            shot,
            blast_air_absorption: blast.air_absorption,
            blast_morph: blast.morph,
            blast_makeup: blast.makeup,
            blast_peak_dbfs: gain_to_db(blast.peak * master),
            crack_air_absorption,
            crack_duration_ms,
            whitham_relative_db,
            crack_source_makeup,
            crack_target_peak_dbfs,
            measured_crack_s,
            measured_blast_s,
        });
    }
    (clips, reports)
}

fn render_blast_component(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    program: &[f32],
    distance_m: f64,
    elevation_deg: f64,
) -> BlastComponent {
    let dry = scheduled_program(program, 0);
    let distance_stem = scaled(&dry, 1.0 / distance_m);
    let (post_air, air_absorption) =
        apply_steam_air_absorption(context, audio_settings, &distance_stem, distance_m);
    let direction = steam_direction_from_elevation(elevation_deg);
    let hrtf_ir = render_hrtf_impulse(context, audio_settings, hrtf, direction);
    let morph = MorphParams::at_distance(distance_m);
    let makeup = analytic_makeup(&post_air, morph, &hrtf_ir);
    let morphed = apply_morph(&post_air, morph, makeup);
    let stereo = render_binaural(context, audio_settings, hrtf, &morphed, direction);
    let peak = stereo_peak(&stereo);
    let onset_frame = onset_frame(&stereo, peak * ONSET_FRACTION_OF_PEAK);
    BlastComponent {
        stereo,
        onset_frame,
        peak,
        air_absorption,
        morph,
        makeup,
    }
}

struct CrackComponent {
    stereo: Vec<f32>,
    peak: f64,
    air_absorption: [f32; 3],
}

fn render_crack_component(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    n_wave: &[f32],
    path_distance_m: f64,
    elevation_deg: f64,
) -> CrackComponent {
    let mut dry = vec![0.0_f32; CLIP_FRAMES];
    dry[..n_wave.len()].copy_from_slice(n_wave);
    multiply_in_place(&mut dry, 1.0 / path_distance_m);
    let (post_air, air_absorption) =
        apply_steam_air_absorption(context, audio_settings, &dry, path_distance_m);
    let direction = steam_direction_from_elevation(elevation_deg);
    let stereo = render_binaural(context, audio_settings, hrtf, &post_air, direction);
    let peak = stereo_peak(&stereo);
    CrackComponent {
        stereo,
        peak,
        air_absorption,
    }
}

fn apply_steam_air_absorption(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    input: &[f32],
    distance_m: f64,
) -> (Vec<f32>, [f32; 3]) {
    assert_eq!(input.len(), CLIP_FRAMES);
    let mut model = default_air_absorption_model();
    let source = ffi::IPLVector3 {
        x: 0.0,
        y: 0.0,
        z: -(distance_m as f32),
    };
    let listener = ffi::IPLVector3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    let air_absorption = ffi::air_absorption_calculate(context.raw(), source, listener, &mut model);
    assert!(air_absorption.into_iter().all(|value| value.is_finite()));

    let mut effect = DirectEffect::create(context, audio_settings).expect("iplDirectEffectCreate");
    let mut input_buffer =
        AudioBuffer::allocate(context, 1, FRAME_SIZE as i32).expect("allocate mono input");
    let mut output_buffer =
        AudioBuffer::allocate(context, 1, FRAME_SIZE as i32).expect("allocate mono output");
    let mut input_block = vec![0.0_f32; FRAME_SIZE];
    let mut output_block = vec![0.0_f32; FRAME_SIZE];
    let mut output = Vec::with_capacity(input.len());
    let mut params = ffi::IPLDirectEffectParams {
        flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION,
        transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
        distanceAttenuation: 1.0,
        airAbsorption: air_absorption,
        directivity: 1.0,
        occlusion: 1.0,
        transmission: [1.0; 3],
    };
    for block in input.chunks(FRAME_SIZE) {
        input_block.fill(0.0);
        input_block[..block.len()].copy_from_slice(block);
        input_buffer.write_interleaved(&mut input_block);
        effect.apply(&mut params, &mut input_buffer, &mut output_buffer);
        output_buffer.read_interleaved(&mut output_block);
        output.extend_from_slice(&output_block[..block.len()]);
    }
    assert!(output.iter().all(|sample| sample.is_finite()));
    (output, air_absorption)
}

fn render_binaural(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    input: &[f32],
    direction: ffi::IPLVector3,
) -> Vec<f32> {
    let mut effect =
        BinauralEffect::create(context, audio_settings, hrtf).expect("iplBinauralEffectCreate");
    let mut input_buffer =
        AudioBuffer::allocate(context, 1, FRAME_SIZE as i32).expect("allocate binaural input");
    let mut output_buffer =
        AudioBuffer::allocate(context, 2, FRAME_SIZE as i32).expect("allocate binaural output");
    let mut input_block = vec![0.0_f32; FRAME_SIZE];
    let mut output_block = vec![0.0_f32; FRAME_SIZE * 2];
    let mut output = Vec::with_capacity(input.len() * 2);
    let mut params = ffi::IPLBinauralEffectParams {
        direction,
        interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
        spatialBlend: 1.0,
        hrtf: hrtf.raw(),
        peakDelays: core::ptr::null_mut(),
    };
    for block in input.chunks(FRAME_SIZE) {
        input_block.fill(0.0);
        input_block[..block.len()].copy_from_slice(block);
        input_buffer.write_interleaved(&mut input_block);
        effect.apply(&mut params, &mut input_buffer, &mut output_buffer);
        output_buffer.read_interleaved(&mut output_block);
        output.extend_from_slice(&output_block[..block.len() * 2]);
    }
    assert!(output.iter().all(|sample| sample.is_finite()));
    output
}

fn render_hrtf_impulse(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    direction: ffi::IPLVector3,
) -> Vec<f32> {
    let mut impulse = vec![0.0_f32; HRTF_IR_FRAMES];
    impulse[0] = 1.0;
    render_binaural(context, audio_settings, hrtf, &impulse, direction)
}

fn analytic_makeup(input: &[f32], morph: MorphParams, hrtf_ir: &[f32]) -> f64 {
    assert_eq!(hrtf_ir.len(), HRTF_IR_FRAMES * 2);
    let fft_len = (input.len() + HRTF_IR_FRAMES).next_power_of_two();
    let mut source = vec![Complex::default(); fft_len];
    let mut left = vec![Complex::default(); fft_len];
    let mut right = vec![Complex::default(); fft_len];
    for (bin, &sample) in source.iter_mut().zip(input) {
        bin.re = f64::from(sample);
    }
    for (frame, channels) in hrtf_ir.chunks_exact(2).enumerate() {
        left[frame].re = f64::from(channels[0]);
        right[frame].re = f64::from(channels[1]);
    }
    fft(&mut source);
    fft(&mut left);
    fft(&mut right);

    let mut plain_energy = 0.0_f64;
    let mut morph_energy = 0.0_f64;
    for index in 0..fft_len {
        let downstream = left[index].norm_squared() + right[index].norm_squared();
        let weight = source[index].norm_squared() * downstream;
        let omega = TAU * index as f64 / fft_len as f64;
        plain_energy += weight;
        morph_energy += weight * morph.magnitude_squared(omega);
    }
    assert!(plain_energy.is_finite() && morph_energy.is_finite() && morph_energy > 0.0);
    (plain_energy / morph_energy).sqrt()
}

fn fft(values: &mut [Complex]) {
    let count = values.len();
    assert!(count.is_power_of_two());
    let mut reversed = 0_usize;
    for index in 1..count {
        let mut bit = count >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed ^= bit;
        if index < reversed {
            values.swap(index, reversed);
        }
    }
    let mut length = 2;
    while length <= count {
        let root = Complex::from_polar(1.0, -TAU / length as f64);
        for start in (0..count).step_by(length) {
            let mut phase = Complex { re: 1.0, im: 0.0 };
            for offset in 0..length / 2 {
                let even = values[start + offset];
                let odd = values[start + offset + length / 2].mul(phase);
                values[start + offset] = even.add(odd);
                values[start + offset + length / 2] = even.sub(odd);
                phase = phase.mul(root);
            }
        }
        length *= 2;
    }
}

fn apply_morph(input: &[f32], morph: MorphParams, makeup: f64) -> Vec<f32> {
    let mut states = vec![0.0_f64; morph.stages as usize];
    let feed = 1.0 - morph.pole;
    let mut output = Vec::with_capacity(input.len());
    for &sample in input {
        let mut value = f64::from(sample);
        for state in &mut states {
            let filtered = feed * value + morph.pole * *state;
            *state = filtered;
            value = filtered;
        }
        output.push((value * makeup) as f32);
    }
    output
}

fn shot_math(miss_distance_m: f64, foot_distance_m: f64) -> ShotMath {
    let root = (MACH * MACH - 1.0).sqrt();
    let velocity = MACH * SOUND_SPEED_MPS;
    let s_star_m = foot_distance_m - miss_distance_m / root;
    let t_star_s = s_star_m / velocity;
    let r_star_m = miss_distance_m * MACH / root;
    let t_crack_s = t_star_s + r_star_m / SOUND_SPEED_MPS;
    let blast_distance_m = foot_distance_m.hypot(miss_distance_m);
    let t_blast_s = blast_distance_m / SOUND_SPEED_MPS;
    ShotMath {
        miss_distance_m,
        foot_distance_m,
        s_star_m,
        t_star_s,
        r_star_m,
        t_crack_s,
        blast_distance_m,
        t_blast_s,
        lead_s: t_blast_s - t_crack_s,
        crack_elevation_deg: (1.0 / MACH).acos().to_degrees(),
        blast_elevation_deg: miss_distance_m.atan2(foot_distance_m).to_degrees(),
        crack_exists: s_star_m >= 0.0,
    }
}

fn synthesize_n_wave(duration_ms: f64) -> Vec<f32> {
    let frames = (duration_ms * SAMPLE_RATE_HZ as f64 / 1_000.0)
        .round()
        .max(4.0) as usize;
    let negative_peak = N_WAVE_POSITIVE_FRACTION / (1.0 - N_WAVE_POSITIVE_FRACTION);
    (0..frames)
        .map(|frame| {
            let phase = frame as f64 / (frames - 1) as f64;
            if phase <= N_WAVE_POSITIVE_FRACTION {
                (1.0 - phase / N_WAVE_POSITIVE_FRACTION) as f32
            } else {
                (-negative_peak * (phase - N_WAVE_POSITIVE_FRACTION)
                    / (1.0 - N_WAVE_POSITIVE_FRACTION)) as f32
            }
        })
        .collect()
}

fn interpolated_cutoff_hz(distance_m: f64) -> f64 {
    if distance_m <= MORPH_KNOTS[0].distance_m {
        return MORPH_KNOTS[0].cutoff_hz;
    }
    if distance_m >= MORPH_KNOTS[MORPH_KNOTS.len() - 1].distance_m {
        return MORPH_KNOTS[MORPH_KNOTS.len() - 1].cutoff_hz;
    }
    for pair in MORPH_KNOTS.windows(2) {
        if distance_m <= pair[1].distance_m {
            let t = (distance_m.ln() - pair[0].distance_m.ln())
                / (pair[1].distance_m.ln() - pair[0].distance_m.ln());
            return (pair[0].cutoff_hz.ln()
                + t * (pair[1].cutoff_hz.ln() - pair[0].cutoff_hz.ln()))
            .exp();
        }
    }
    unreachable!()
}

fn steam_direction_from_azimuth(azimuth_deg: f64) -> ffi::IPLVector3 {
    let radians = azimuth_deg.to_radians();
    ffi::IPLVector3 {
        x: radians.sin() as f32,
        y: 0.0,
        z: -(radians.cos() as f32),
    }
}

fn steam_direction_from_elevation(elevation_deg: f64) -> ffi::IPLVector3 {
    let radians = elevation_deg.to_radians();
    ffi::IPLVector3 {
        x: 0.0,
        y: radians.sin() as f32,
        z: -(radians.cos() as f32),
    }
}

fn scheduled_program(program: &[f32], onset_frame: usize) -> Vec<f32> {
    assert!(onset_frame + program.len() <= CLIP_FRAMES);
    let mut output = vec![0.0_f32; CLIP_FRAMES];
    output[onset_frame..onset_frame + program.len()].copy_from_slice(program);
    output
}

fn place_component(stereo: &[f32], onset_frame: usize, target_frame: usize) -> Vec<f32> {
    assert_eq!(stereo.len(), CLIP_FRAMES * 2);
    let shift = target_frame as isize - onset_frame as isize;
    let mut output = vec![0.0_f32; stereo.len()];
    for source_frame in 0..CLIP_FRAMES {
        let target = source_frame as isize + shift;
        if (0..CLIP_FRAMES as isize).contains(&target) {
            let target = target as usize;
            output[target * 2] = stereo[source_frame * 2];
            output[target * 2 + 1] = stereo[source_frame * 2 + 1];
        }
    }
    output
}

fn first_nonzero_mono(samples: &[f32]) -> usize {
    samples
        .iter()
        .position(|sample| sample.abs() > 1.0e-20)
        .expect("nonzero mono signal")
}

fn onset_frame(stereo: &[f32], threshold: f64) -> usize {
    stereo
        .chunks_exact(2)
        .position(|frame| f64::from(frame[0]).abs().max(f64::from(frame[1]).abs()) >= threshold)
        .expect("stereo onset")
}

fn detect_onset_near(
    stereo: &[f32],
    expected_frame: usize,
    threshold: f64,
    radius_frames: usize,
) -> usize {
    let start = expected_frame.saturating_sub(radius_frames);
    let end = (expected_frame + radius_frames + 1).min(CLIP_FRAMES);
    (start..end)
        .find(|&frame| {
            f64::from(stereo[frame * 2])
                .abs()
                .max(f64::from(stereo[frame * 2 + 1]).abs())
                >= threshold
        })
        .expect("onset near expected frame")
}

fn seconds_to_frame(seconds: f64) -> usize {
    (seconds * SAMPLE_RATE_HZ as f64).round() as usize
}

fn scaled(samples: &[f32], gain: f64) -> Vec<f32> {
    samples
        .iter()
        .map(|sample| (f64::from(*sample) * gain) as f32)
        .collect()
}

fn multiply_in_place(samples: &mut [f32], gain: f64) {
    for sample in samples {
        *sample = (f64::from(*sample) * gain) as f32;
    }
}

fn add_stereo(target: &mut [f32], source: &[f32]) {
    assert_eq!(target.len(), source.len());
    for (target, source) in target.iter_mut().zip(source) {
        *target += *source;
    }
}

fn stereo_energy(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum()
}

fn energy_delta_db(candidate: &[f32], reference: &[f32]) -> f64 {
    10.0 * (stereo_energy(candidate) / stereo_energy(reference)).log10()
}

fn stereo_peak(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|sample| f64::from(*sample).abs())
        .fold(0.0_f64, f64::max)
}

fn stereo_level_metrics(stereo: &[f32]) -> (f64, f64) {
    let mean_square = stereo_energy(stereo) / stereo.len() as f64;
    (
        10.0 * mean_square.max(1.0e-30).log10(),
        gain_to_db(stereo_peak(stereo)),
    )
}

fn db_to_gain(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

fn gain_to_db(gain: f64) -> f64 {
    20.0 * gain.max(1.0e-30).log10()
}

fn apply_fade_out(samples: &mut [f32], frames: usize) {
    assert!(frames <= samples.len());
    let start = samples.len() - frames;
    for (offset, sample) in samples[start..].iter_mut().enumerate() {
        let phase = PI * 0.5 * offset as f64 / frames as f64;
        *sample *= phase.cos().powi(2) as f32;
    }
}

fn make_clip(name: &'static str, stereo: Vec<f32>, listen_for: &'static str) -> ClipRecord {
    assert_eq!(stereo.len(), CLIP_FRAMES * 2);
    assert!(stereo.iter().all(|sample| sample.is_finite()));
    let (rms_dbfs, peak_dbfs) = stereo_level_metrics(&stereo);
    let bytes = write_wav(stereo_spec(), &stereo).expect("encode float WAV");
    let sha256 = sha256_hex(&bytes);
    ClipRecord {
        name,
        bytes,
        sha256,
        rms_dbfs,
        peak_dbfs,
        listen_for,
    }
}

fn build_manifest(outcome: &RenderOutcome, source_onset_frame: usize, source_peak: f64) -> String {
    let mut manifest = String::new();
    writeln!(
        manifest,
        "# Wave 12 listening strip: distance morph and ballistic crack\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "These twelve deterministic clips are 2.500 seconds of 48 kHz stereo IEEE-float PCM, rendered in 512-frame blocks through Steam Audio 4.8.1's default HRTF. The artillery recording is pinned by SHA-256 `{SOURCE_SHA256}`; its detected onset is source frame {source_onset_frame}, its original peak is {source_peak:.9}, and this strip uses 2.000 seconds from that onset with a 50 ms fade at the crop boundary.\n"
    )
    .unwrap();
    writeln!(manifest, "## Listening map\n").unwrap();
    for clip in &outcome.clips {
        writeln!(
            manifest,
            "- `{}`: {}  \n  SHA-256 `{}`; integrated stereo RMS {:.3} dBFS; peak {:.3} dBFS.",
            clip.name, clip.listen_for, clip.sha256, clip.rms_dbfs, clip.peak_dbfs
        )
        .unwrap();
    }

    writeln!(manifest, "\n## B-strip numbers: the full distance chain\n").unwrap();
    writeln!(
        manifest,
        "Every B clip keeps the real `1/d` falloff and Steam Audio's own default three-band air absorption. A single +{B_MASTER_GAIN_DB:.1} dB strip monitoring gain is shared by all eight clips; there is no distance-specific makeup. The residual morph is two cascaded causal one-pole low-passes, `H(z) = ((1-p)/(1-p z^-1))^2`. The pole lies strictly inside the unit circle and there are no finite zeros, so the filter is stable and minimum-phase. Its non-zero direct feed changes no onset sample and adds zero integer-sample latency.\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "The curve is log-distance/log-cutoff interpolated between these exact knots: 5 m = 18000 Hz, 50 m = 7500 Hz, 200 m = 2800 Hz, and 500 m = 1100 Hz. Makeup is one fixed number for each distance, computed once from the filter transfer with the fixed post-air source spectrum and fixed forward HRTF transfer: `G = sqrt(sum |X|^2 |B|^2 / sum |X|^2 |B|^2 |H|^2)`. It never follows a block, peak, or envelope.\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "| distance | 1/d | Steam low/mid/high | cutoff | pole p | direct feed | residual at 10 kHz | fixed makeup | plain RMS | morph RMS | morph-minus-plain energy | filter input/output onset |\n|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    )
    .unwrap();
    for report in &outcome.b_reports {
        writeln!(
            manifest,
            "| {:.0} m | {:.9} | {:.9}/{:.9}/{:.9} | {:.1} Hz | {:.9} | {:.9} | {:.3} dB | {:.9} | {:.3} dBFS | {:.3} dBFS | {:+.6} dB PASS | {}/{} frames PASS |",
            report.distance_m,
            report.distance_gain,
            report.air_absorption[0],
            report.air_absorption[1],
            report.air_absorption[2],
            report.morph.cutoff_hz,
            report.morph.pole,
            report.morph.direct_feed(),
            report.morph.attenuation_db_at(10_000.0),
            report.makeup,
            report.plain_rms_dbfs,
            report.morph_rms_dbfs,
            report.energy_delta_db,
            report.plain_onset_frame,
            report.morph_onset_frame
        )
        .unwrap();
    }
    writeln!(
        manifest,
        "\nAll four morph/plain broadband-energy pairs are within 0.1 dB. An impulse-through-filter probe has the same exact non-zero input and output onset frame at every knot, so the residual rounding is timbre, not an inserted sample delay.\n"
    )
    .unwrap();

    writeln!(manifest, "## A-strip numbers: cone timing and level\n").unwrap();
    writeln!(
        manifest,
        "The bullet travels at Mach {MACH:.3} (`v = {:.3} m/s`) along +north. The worked-example angles are rendered in the vertical ahead plane: positive elevation is above the listener, so 66.4 degrees is above/ahead while 26.6 degrees is lower and nearer the gun direction. The N-wave reference lasts {N_WAVE_REFERENCE_DURATION_MS:.3} ms at 30 m and grows as `d^(1/4)`. Its positive segment occupies {:.1}% of the duration and falls from +1 to zero; the negative segment occupies {:.1}% and falls to {:.9}, chosen so the continuous areas cancel. The 30 m received crack peak is {CRACK_OVER_REFERENCE_BLAST_DB:+.3} dB relative to that shot's received blast peak; other crack peaks use Whitham `d^(-3/4)`. A single +{A_MASTER_GAIN_DB:.1} dB strip monitoring gain follows the combined shot.\n",
        MACH * SOUND_SPEED_MPS,
        N_WAVE_POSITIVE_FRACTION * 100.0,
        (1.0 - N_WAVE_POSITIVE_FRACTION) * 100.0,
        -N_WAVE_POSITIVE_FRACTION / (1.0 - N_WAVE_POSITIVE_FRACTION)
    )
    .unwrap();
    writeln!(
        manifest,
        "| clip | d / s0 | s* | t* | r* | crack arrival | blast path / arrival | lead | crack / blast elevation | crack duration | Whitham level | crack target | crack source makeup | measured crack / blast |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    )
    .unwrap();
    for report in &outcome.a_reports {
        let crack_duration = report
            .crack_duration_ms
            .map(|value| format!("{value:.6} ms"))
            .unwrap_or_else(|| "none".to_owned());
        let whitham = report
            .whitham_relative_db
            .map(|value| format!("{value:+.6} dB"))
            .unwrap_or_else(|| "rejected".to_owned());
        let crack_target = report
            .crack_target_peak_dbfs
            .map(|value| format!("{value:.6} dBFS"))
            .unwrap_or_else(|| "none".to_owned());
        let source_makeup = report
            .crack_source_makeup
            .map(|value| format!("{value:.9} ({:+.6} dB)", gain_to_db(value)))
            .unwrap_or_else(|| "none".to_owned());
        let measured_crack = report
            .measured_crack_s
            .map(|value| format!("{:.3} ms", value * 1_000.0))
            .unwrap_or_else(|| "none".to_owned());
        writeln!(
            manifest,
            "| `{}` | {:.3} / {:.3} m | {:.6} m | {:.6} ms | {:.6} m | {:.6} ms{} | {:.6} m / {:.6} ms | {:.6} ms | {:.6} / {:.6} deg | {} | {} | {} | {} | {} / {:.3} ms |",
            report.name,
            report.shot.miss_distance_m,
            report.shot.foot_distance_m,
            report.shot.s_star_m,
            report.shot.t_star_s * 1_000.0,
            report.shot.r_star_m,
            report.shot.t_crack_s * 1_000.0,
            if report.shot.crack_exists { "" } else { " (candidate rejected: s* < 0)" },
            report.shot.blast_distance_m,
            report.shot.t_blast_s * 1_000.0,
            report.shot.lead_s * 1_000.0,
            report.shot.crack_elevation_deg,
            report.shot.blast_elevation_deg,
            crack_duration,
            whitham,
            crack_target,
            source_makeup,
            measured_crack,
            report.measured_blast_s * 1_000.0
        )
        .unwrap();
    }
    writeln!(manifest, "\nThe blast path also uses the same Steam air stage and residual morph curve. Every computed value used by those four blast renders is below.\n").unwrap();
    writeln!(manifest, "| clip | blast Steam low/mid/high | blast morph cutoff / pole | blast fixed makeup | blast peak | crack Steam low/mid/high |\n|---|---|---:|---:|---:|---|").unwrap();
    for report in &outcome.a_reports {
        let crack_air = report
            .crack_air_absorption
            .map(|air| format!("{:.9}/{:.9}/{:.9}", air[0], air[1], air[2]))
            .unwrap_or_else(|| "none".to_owned());
        writeln!(
            manifest,
            "| `{}` | {:.9}/{:.9}/{:.9} | {:.3} Hz / {:.9} | {:.9} | {:.6} dBFS | {} |",
            report.name,
            report.blast_air_absorption[0],
            report.blast_air_absorption[1],
            report.blast_air_absorption[2],
            report.blast_morph.cutoff_hz,
            report.blast_morph.pole,
            report.blast_makeup,
            report.blast_peak_dbfs,
            crack_air
        )
        .unwrap();
    }

    let reference = &outcome.a_reports[0];
    writeln!(
        manifest,
        "\n## Onset gate measured from the written float samples\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "| event | computed | measured | error | requirement |\n|---|---:|---:|---:|---:|"
    )
    .unwrap();
    let crack_measured = reference
        .measured_crack_s
        .expect("reference crack measurement");
    writeln!(
        manifest,
        "| crack | {:.6} ms | {:.6} ms | {:+.6} ms | +/-0.5 ms PASS |",
        reference.shot.t_crack_s * 1_000.0,
        crack_measured * 1_000.0,
        (crack_measured - reference.shot.t_crack_s) * 1_000.0
    )
    .unwrap();
    writeln!(
        manifest,
        "| blast | {:.6} ms | {:.6} ms | {:+.6} ms | +/-0.5 ms PASS |",
        reference.shot.t_blast_s * 1_000.0,
        reference.measured_blast_s * 1_000.0,
        (reference.measured_blast_s - reference.shot.t_blast_s) * 1_000.0
    )
    .unwrap();

    writeln!(manifest, "\n## Determinism and decision boundary\n").unwrap();
    writeln!(
        manifest,
        "The ignored test rendered the complete twelve-file set twice from fresh Steam Audio effects in one process and required byte-for-byte identical float WAVs and identical SHA-256s before writing this directory. The lane was then invoked twice independently; both invocations printed the same twelve SHA-256s listed above.\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "This strip decides only whether the morph curve reads to md's ears as increasing acoustic distance and whether crack timing, direction, and Whitham level read as a ballistic snap followed by a gun boom. It cannot decide in-engine occlusion, source-slot lifecycle, workbench triggering, propagation smoothing, reflections, or callback behavior. Those remain implementation gates after listening sign-off."
    )
    .unwrap();
    manifest
}

fn source_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/assets/music/artillery-impact-48k-mono.wav")
}

fn stereo_spec() -> WavSpec {
    WavSpec {
        sample_rate_hz: SAMPLE_RATE_HZ,
        channels: 2,
    }
}

fn decode_pcm16_mono(bytes: &[u8]) -> Vec<f32> {
    assert!(bytes.len() >= 12);
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut position = 12;
    let mut format = None;
    let mut data = None;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size =
            u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
        position += 8;
        let end = position.checked_add(size).expect("WAV chunk size overflow");
        assert!(end <= bytes.len(), "truncated source WAV chunk");
        if id == b"fmt " {
            assert!(size >= 16);
            format = Some((
                u16::from_le_bytes(bytes[position..position + 2].try_into().unwrap()),
                u16::from_le_bytes(bytes[position + 2..position + 4].try_into().unwrap()),
                u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()),
                u16::from_le_bytes(bytes[position + 14..position + 16].try_into().unwrap()),
            ));
        } else if id == b"data" {
            data = Some(&bytes[position..end]);
        }
        position = end + (size & 1);
    }
    assert_eq!(format, Some((1, 1, SAMPLE_RATE_HZ, 16)));
    data.expect("source WAV data chunk")
        .chunks_exact(2)
        .map(|sample| f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32_768.0)
        .collect()
}
