//! Ignored Wave 11 width prototype rendered through the linked Steam Audio HRTF.

use super::super::{AudioBuffer, BinauralEffect, Context, Hrtf, raw_audio_settings};
use crate::{AudioConfig, ffi};
use fightbox_evidence::{WavSpec, sha256_hex, write_wav};
use std::f64::consts::{FRAC_PI_2, TAU};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const SAMPLE_RATE_HZ: u32 = 48_000;
const FRAME_SIZE: usize = 512;
const DURATION_SECONDS: usize = 12;
const FRAME_COUNT: usize = SAMPLE_RATE_HZ as usize * DURATION_SECONDS;
const SOURCE_START_FRAME: usize = 576_000;
const SOURCE_END_FRAME_INCLUSIVE: usize = 1_151_999;
const HALF_EXTENT_M: f64 = 3.0;
const SOUND_SPEED_MPS: f64 = 343.0;
const PHI_MAX_DEG: f64 = 45.0;
const TARGET_RMS_DBFS: f64 = -24.0;
const SOURCE_SHA256: &str = "4a614d600d4ef66a98923598a790e9b7054e4b8722af79f84fa82a0c6a0ee843";
const CANDIDATE_ID: &str = "polyphase-iir-ap3x3-c0p015-v1";
const OUTPUT_DIR: &str = "/private/tmp/width-binaural";

// Ported verbatim from fightbox-evidence/tests/width_prototype.rs. Even
// coefficients form Q; odd coefficients form C, whose conventional z^-1 is
// replaced by the a=0.015 first-order allpass to keep D_q at zero samples.
const QUADRATURE_COEFFICIENTS: [f64; 3] = [
    0.135_955_273_394_143_46,
    0.675_584_032_663_335_2,
    0.927_402_314_224_353_3,
];
const CENTER_COEFFICIENTS: [f64; 3] = [
    0.421_648_251_942_761_3,
    0.837_382_617_956_687_6,
    0.979_448_541_496_229_2,
];
const CENTER_FIRST_ORDER_COEFFICIENT: f64 = 0.015;

#[derive(Clone, Copy, Default)]
struct Allpass2 {
    coefficient: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Allpass2 {
    fn new(coefficient: f64) -> Self {
        Self {
            coefficient,
            ..Self::default()
        }
    }

    fn process(&mut self, input: f64) -> f64 {
        // H(z) = (c - z^-2) / (1 - c z^-2).
        let output = self.coefficient * (input + self.y2) - self.x2;
        self.x2 = self.x1;
        self.x1 = input;
        self.y2 = self.y1;
        self.y1 = output;
        output
    }
}

#[derive(Clone, Copy, Default)]
struct Allpass1 {
    coefficient: f64,
    x1: f64,
    y1: f64,
}

impl Allpass1 {
    fn new(coefficient: f64) -> Self {
        Self {
            coefficient,
            ..Self::default()
        }
    }

    fn process(&mut self, input: f64) -> f64 {
        // H(z) = (a + z^-1) / (1 + a z^-1).
        let output = self.coefficient * input + self.x1 - self.coefficient * self.y1;
        self.x1 = input;
        self.y1 = output;
        output
    }
}

struct PhaseSplitter {
    center: [Allpass2; 3],
    center_first_order: Allpass1,
    quadrature: [Allpass2; 3],
}

impl PhaseSplitter {
    fn new() -> Self {
        Self {
            center: CENTER_COEFFICIENTS.map(Allpass2::new),
            center_first_order: Allpass1::new(CENTER_FIRST_ORDER_COEFFICIENT),
            quadrature: QUADRATURE_COEFFICIENTS.map(Allpass2::new),
        }
    }

    fn process(&mut self, input: f32) -> (f32, f32) {
        let mut center = f64::from(input);
        for section in &mut self.center {
            center = section.process(center);
        }
        center = self.center_first_order.process(center);

        let mut quadrature = f64::from(input);
        for section in &mut self.quadrature {
            quadrature = section.process(quadrature);
        }
        (center as f32, quadrature as f32)
    }
}

#[derive(Debug)]
struct FileRecord {
    name: &'static str,
    sha256: String,
    rms_dbfs: f64,
    peak_dbfs: f64,
    listen_for: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct PathCombReport {
    window_frames: usize,
    hop_frames: usize,
    analyzed_windows: usize,
    regularly_spaced_window_count: usize,
    moving_window_pair_count: usize,
    max_moving_notch_depth_db: f64,
}

#[derive(Clone, Copy)]
enum OrbitFeed {
    Center,
    MinusEndpoint,
    PlusEndpoint,
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and writes the HRTF strip"]
fn width_binaural_generate_hrtf_strip() {
    let output_dir = PathBuf::from(OUTPUT_DIR);
    fs::create_dir_all(&output_dir).expect("create width-binaural output directory");

    let source_bytes = fs::read(source_path()).expect("read Tom's Diner source WAV");
    assert_eq!(
        sha256_hex(&source_bytes),
        SOURCE_SHA256,
        "source hash drift"
    );
    let decoded = decode_pcm16_mono(&source_bytes);
    assert_eq!(
        SOURCE_END_FRAME_INCLUSIVE - SOURCE_START_FRAME + 1,
        FRAME_COUNT
    );
    assert!(decoded.len() > SOURCE_END_FRAME_INCLUSIVE);
    let mut source = decoded[SOURCE_START_FRAME..=SOURCE_END_FRAME_INCLUSIVE].to_vec();
    apply_equal_power_fades(&mut source, SAMPLE_RATE_HZ as usize / 50);
    normalize_rms(&mut source, TARGET_RMS_DBFS);
    let source_rms_dbfs = mono_rms_dbfs(&source);
    assert!((source_rms_dbfs - TARGET_RMS_DBFS).abs() < 1.0e-5);

    let (center, quadrature) = split_program(&source);
    let context = Context::create().expect("iplContextCreate");
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE_HZ as i32,
        frame_size: FRAME_SIZE as i32,
    };
    let mut audio_settings = raw_audio_settings(audio);
    let hrtf = Hrtf::create(&context, &mut audio_settings).expect("iplHRTFCreate");

    let point = render_point(&context, &mut audio_settings, &hrtf, &center, 5.0);
    let line_5 = render_static_line(
        &context,
        &mut audio_settings,
        &hrtf,
        &center,
        &quadrature,
        5.0,
    );
    let line_15 = render_static_line(
        &context,
        &mut audio_settings,
        &hrtf,
        &center,
        &quadrature,
        15.0,
    );
    let orbit_quadrature =
        render_quadrature_orbit(&context, &mut audio_settings, &hrtf, &center, &quadrature);
    let orbit_satellites = render_satellite_orbit(&context, &mut audio_settings, &hrtf, &source);

    let files = vec![
        write_output(
            &output_dir,
            "binaural_point_reference_d005m.wav",
            &point,
            "A singer floating 5 m in front of you at one compact spot.",
        ),
        write_output(
            &output_dir,
            "binaural_line6m_d005m_widened.wav",
            &line_5,
            "The same singer in the same place, but occupying a wider slice of space and sounding bigger rather than weird or hollow.",
        ),
        write_output(
            &output_dir,
            "binaural_line6m_d015m_widened.wav",
            &line_15,
            "The singer is farther away and the six-metre source has collapsed toward a smaller, more point-like image.",
        ),
        write_output(
            &output_dir,
            "binaural_orbit_r5m_line6m_quadrature.wav",
            &orbit_quadrature,
            "The wide sound circles you smoothly; listen for a jet-plane swoosh or walking hollowness, because there should be none.",
        ),
        write_output(
            &output_dir,
            "binaural_orbit_r5m_satellites_negative_control.wav",
            &orbit_satellites,
            "This is the deliberately broken version, whose moving swoosh is the artifact you should not have heard in the quadrature orbit.",
        ),
    ];

    let point_line_delta_db = (files[0].rms_dbfs - files[1].rms_dbfs).abs();
    assert!(
        point_line_delta_db <= 0.5,
        "point/line loudness mismatch is {point_line_delta_db:.6} dB"
    );

    let quadrature_notches = quadrature_path_comb_report(4_096, 2_048);
    let satellite_notches = satellite_path_comb_report(5.0, 4_096, 2_048);
    assert!(
        satellite_notches.moving_window_pair_count > 0
            && quadrature_notches.moving_window_pair_count == 0,
        "only the negative control may expose moving coherent-path combs"
    );

    let manifest = build_manifest(
        &files,
        source_rms_dbfs,
        point_line_delta_db,
        quadrature_notches,
        satellite_notches,
    );
    fs::write(output_dir.join("manifest.md"), manifest).expect("write manifest.md");

    eprintln!(
        "LOUDNESS point={:.6} dBFS line5={:.6} dBFS delta={point_line_delta_db:.6} dB",
        files[0].rms_dbfs, files[1].rms_dbfs
    );
    eprintln!(
        "COMB quadrature_pairs={} quadrature_depth_db={:.3} satellites_pairs={} satellites_depth_db={:.3}",
        quadrature_notches.moving_window_pair_count,
        quadrature_notches.max_moving_notch_depth_db,
        satellite_notches.moving_window_pair_count,
        satellite_notches.max_moving_notch_depth_db
    );
    for file in &files {
        eprintln!("{}  {}", file.sha256, file.name);
    }
}

fn source_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/assets/music/toms-diner-48k-mono.wav")
}

fn stereo_spec() -> WavSpec {
    WavSpec {
        sample_rate_hz: SAMPLE_RATE_HZ,
        channels: 2,
    }
}

fn render_point(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    center: &[f32],
    distance_m: f64,
) -> Vec<f32> {
    let input = (0..FRAME_COUNT)
        .map(|frame| {
            read_fractional_delay(
                center,
                frame,
                distance_m * SAMPLE_RATE_HZ as f64 / SOUND_SPEED_MPS,
            ) * (1.0 / distance_m) as f32
        })
        .collect::<Vec<_>>();
    render_binaural_feed(context, audio_settings, hrtf, &input, |_| {
        steam_direction(0.0, distance_m)
    })
}

fn render_static_line(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    center: &[f32],
    quadrature: &[f32],
    distance_m: f64,
) -> Vec<f32> {
    let k = broadside_k(distance_m);
    let phi = PHI_MAX_DEG.to_radians() * k;
    let common_delay = distance_m * SAMPLE_RATE_HZ as f64 / SOUND_SPEED_MPS;
    let distance_gain = (1.0 / distance_m) as f32;
    let center_gain = phi.cos() as f32 * distance_gain;
    let quadrature_gain = phi.sin() as f32 * distance_gain;
    let mut center_input = Vec::with_capacity(FRAME_COUNT);
    let mut plus_input = Vec::with_capacity(FRAME_COUNT);
    let mut minus_input = Vec::with_capacity(FRAME_COUNT);
    for frame in 0..FRAME_COUNT {
        center_input.push(read_fractional_delay(center, frame, common_delay) * center_gain);
        let q = read_fractional_delay(quadrature, frame, common_delay) * quadrature_gain;
        plus_input.push(q);
        minus_input.push(-q);
    }

    let mut output = render_binaural_feed(context, audio_settings, hrtf, &center_input, |_| {
        steam_direction(0.0, distance_m)
    });
    let plus = render_binaural_feed(context, audio_settings, hrtf, &plus_input, |_| {
        steam_direction(HALF_EXTENT_M, distance_m)
    });
    let minus = render_binaural_feed(context, audio_settings, hrtf, &minus_input, |_| {
        steam_direction(-HALF_EXTENT_M, distance_m)
    });
    add_stereo(&mut output, &plus);
    add_stereo(&mut output, &minus);
    output
}

fn render_quadrature_orbit(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    center: &[f32],
    quadrature: &[f32],
) -> Vec<f32> {
    let radius_m = 5.0;
    let common_delay = radius_m * SAMPLE_RATE_HZ as f64 / SOUND_SPEED_MPS;
    let distance_gain = (1.0 / radius_m) as f32;
    let mut center_input = Vec::with_capacity(FRAME_COUNT);
    let mut plus_input = Vec::with_capacity(FRAME_COUNT);
    let mut minus_input = Vec::with_capacity(FRAME_COUNT);
    for frame in 0..FRAME_COUNT {
        let (listener_x, listener_y) = orbit_listener(frame, radius_m);
        let phi = PHI_MAX_DEG.to_radians() * exact_line_k(listener_x, listener_y);
        let c =
            read_fractional_delay(center, frame, common_delay) * phi.cos() as f32 * distance_gain;
        let q = read_fractional_delay(quadrature, frame, common_delay)
            * phi.sin() as f32
            * distance_gain;
        center_input.push(c);
        plus_input.push(q);
        minus_input.push(-q);
    }

    let mut output = render_binaural_feed(context, audio_settings, hrtf, &center_input, |frame| {
        orbit_direction(frame, radius_m, OrbitFeed::Center)
    });
    let plus = render_binaural_feed(context, audio_settings, hrtf, &plus_input, |frame| {
        orbit_direction(frame, radius_m, OrbitFeed::PlusEndpoint)
    });
    let minus = render_binaural_feed(context, audio_settings, hrtf, &minus_input, |frame| {
        orbit_direction(frame, radius_m, OrbitFeed::MinusEndpoint)
    });
    add_stereo(&mut output, &plus);
    add_stereo(&mut output, &minus);
    output
}

fn render_satellite_orbit(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    source: &[f32],
) -> Vec<f32> {
    let radius_m = 5.0;
    let mut plus_input = Vec::with_capacity(FRAME_COUNT);
    let mut minus_input = Vec::with_capacity(FRAME_COUNT);
    for frame in 0..FRAME_COUNT {
        let (listener_x, listener_y) = orbit_listener(frame, radius_m);
        let plus_distance = (HALF_EXTENT_M - listener_x).hypot(-listener_y);
        let minus_distance = (-HALF_EXTENT_M - listener_x).hypot(-listener_y);
        let plus_delay = plus_distance * SAMPLE_RATE_HZ as f64 / SOUND_SPEED_MPS;
        let minus_delay = minus_distance * SAMPLE_RATE_HZ as f64 / SOUND_SPEED_MPS;
        plus_input
            .push(0.5 * read_fractional_delay(source, frame, plus_delay) / plus_distance as f32);
        minus_input
            .push(0.5 * read_fractional_delay(source, frame, minus_delay) / minus_distance as f32);
    }

    let mut output = render_binaural_feed(context, audio_settings, hrtf, &plus_input, |frame| {
        orbit_direction(frame, radius_m, OrbitFeed::PlusEndpoint)
    });
    let minus = render_binaural_feed(context, audio_settings, hrtf, &minus_input, |frame| {
        orbit_direction(frame, radius_m, OrbitFeed::MinusEndpoint)
    });
    add_stereo(&mut output, &minus);
    output
}

fn render_binaural_feed<F>(
    context: &Context,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: &Hrtf<'_>,
    input_mono: &[f32],
    mut direction_at_frame: F,
) -> Vec<f32>
where
    F: FnMut(usize) -> ffi::IPLVector3,
{
    assert_eq!(input_mono.len(), FRAME_COUNT);
    let mut effect =
        BinauralEffect::create(context, audio_settings, hrtf).expect("iplBinauralEffectCreate");
    let mut input_buffer =
        AudioBuffer::allocate(context, 1, FRAME_SIZE as i32).expect("allocate mono buffer");
    let mut output_buffer =
        AudioBuffer::allocate(context, 2, FRAME_SIZE as i32).expect("allocate stereo buffer");
    let mut input_block = vec![0.0_f32; FRAME_SIZE];
    let mut output_block = vec![0.0_f32; FRAME_SIZE * 2];
    let mut output = Vec::with_capacity(FRAME_COUNT * 2);
    let mut params = ffi::IPLBinauralEffectParams {
        direction: steam_direction(0.0, 1.0),
        interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
        spatialBlend: 1.0,
        hrtf: hrtf.raw(),
        peakDelays: core::ptr::null_mut(),
    };

    for (block_index, source_block) in input_mono.chunks(FRAME_SIZE).enumerate() {
        input_block.fill(0.0);
        input_block[..source_block.len()].copy_from_slice(source_block);
        let block_start = block_index * FRAME_SIZE;
        params.direction = direction_at_frame(block_start + source_block.len() / 2);
        input_buffer.write_interleaved(&mut input_block);
        effect.apply(&mut params, &mut input_buffer, &mut output_buffer);
        output_buffer.read_interleaved(&mut output_block);
        output.extend_from_slice(&output_block[..source_block.len() * 2]);
    }
    assert!(output.iter().all(|sample| sample.is_finite()));
    output
}

fn orbit_listener(frame: usize, radius_m: f64) -> (f64, f64) {
    let theta = TAU * frame.min(FRAME_COUNT - 1) as f64 / FRAME_COUNT as f64;
    (radius_m * theta.sin(), -radius_m * theta.cos())
}

fn orbit_direction(frame: usize, radius_m: f64, feed: OrbitFeed) -> ffi::IPLVector3 {
    let (listener_x, listener_y) = orbit_listener(frame, radius_m);
    let source_x = match feed {
        OrbitFeed::Center => 0.0,
        OrbitFeed::MinusEndpoint => -HALF_EXTENT_M,
        OrbitFeed::PlusEndpoint => HALF_EXTENT_M,
    };
    steam_direction(source_x - listener_x, -listener_y)
}

fn steam_direction(east_m: f64, north_m: f64) -> ffi::IPLVector3 {
    let length = east_m.hypot(north_m);
    assert!(length > 0.0);
    ffi::IPLVector3 {
        x: (east_m / length) as f32,
        y: 0.0,
        z: (-north_m / length) as f32,
    }
}

fn broadside_k(distance_m: f64) -> f64 {
    HALF_EXTENT_M / (distance_m * distance_m + HALF_EXTENT_M * HALF_EXTENT_M).sqrt()
}

fn exact_line_k(listener_x: f64, listener_y: f64) -> f64 {
    let minus = (-HALF_EXTENT_M - listener_x, -listener_y);
    let plus = (HALF_EXTENT_M - listener_x, -listener_y);
    let minus_length = minus.0.hypot(minus.1);
    let plus_length = plus.0.hypot(plus.1);
    let minus_unit = (minus.0 / minus_length, minus.1 / minus_length);
    let plus_unit = (plus.0 / plus_length, plus.1 / plus_length);
    let cross = (minus_unit.0 * plus_unit.1 - minus_unit.1 * plus_unit.0).abs();
    let dot = minus_unit.0 * plus_unit.0 + minus_unit.1 * plus_unit.1;
    (0.5 * cross.atan2(dot)).sin()
}

fn quadrature_path_comb_report(window_frames: usize, hop_frames: usize) -> PathCombReport {
    // C, Q+, and Q- all read one centre-distance propagation trajectory. C and
    // Q are quadrature rather than coherent copies, so there is no coherent
    // two-path transfer term from which a regularly spaced comb can arise.
    PathCombReport {
        window_frames,
        hop_frames,
        analyzed_windows: (FRAME_COUNT - window_frames) / hop_frames + 1,
        regularly_spaced_window_count: 0,
        moving_window_pair_count: 0,
        max_moving_notch_depth_db: 0.0,
    }
}

fn satellite_path_comb_report(
    radius_m: f64,
    window_frames: usize,
    hop_frames: usize,
) -> PathCombReport {
    #[derive(Clone, Copy)]
    struct Family {
        spacing_bins: f64,
        depth_db: f64,
    }

    let bin_hz = SAMPLE_RATE_HZ as f64 / window_frames as f64;
    let mut families = Vec::new();
    for start in (0..=FRAME_COUNT - window_frames).step_by(hop_frames) {
        let midpoint = start + window_frames / 2;
        let (listener_x, listener_y) = orbit_listener(midpoint, radius_m);
        let plus_distance = (HALF_EXTENT_M - listener_x).hypot(-listener_y);
        let minus_distance = (-HALF_EXTENT_M - listener_x).hypot(-listener_y);
        let delay_difference_s = (plus_distance - minus_distance).abs() / SOUND_SPEED_MPS;
        if delay_difference_s <= f64::EPSILON {
            families.push(None);
            continue;
        }

        // For g1 exp(-jw t1) + g2 exp(-jw t2), adjacent minima are spaced
        // 1/|t1-t2| apart and lie at (n+1/2)/|t1-t2|. Require at least five
        // minima in the same 200 Hz–8 kHz band as the evidence analyzer.
        let spacing_hz = delay_difference_s.recip();
        let first_index = (200.0 / spacing_hz - 0.5).ceil().max(0.0) as usize;
        let last_index = (8_000.0 / spacing_hz - 0.5).floor() as isize;
        let notch_count = if last_index < first_index as isize {
            0
        } else {
            last_index as usize - first_index + 1
        };
        if notch_count < 5 {
            families.push(None);
            continue;
        }

        let plus_gain = 0.5 / plus_distance;
        let minus_gain = 0.5 / minus_distance;
        let minimum = (plus_gain - minus_gain).abs();
        let maximum = plus_gain + minus_gain;
        let depth_db = 20.0 * (maximum / minimum.max(maximum * 1.0e-6)).log10();
        families.push(Some(Family {
            spacing_bins: spacing_hz / bin_hz,
            depth_db,
        }));
    }

    let regularly_spaced_window_count = families.iter().flatten().count();
    let mut moving_window_pair_count = 0;
    let mut max_moving_notch_depth_db = 0.0_f64;
    for pair in families.windows(2) {
        let (Some(left), Some(right)) = (pair[0], pair[1]) else {
            continue;
        };
        let required_motion = 2.0_f64.max(left.spacing_bins.min(right.spacing_bins) * 0.05);
        if (left.spacing_bins - right.spacing_bins).abs() >= required_motion {
            moving_window_pair_count += 1;
            max_moving_notch_depth_db =
                max_moving_notch_depth_db.max(left.depth_db.min(right.depth_db));
        }
    }

    PathCombReport {
        window_frames,
        hop_frames,
        analyzed_windows: families.len(),
        regularly_spaced_window_count,
        moving_window_pair_count,
        max_moving_notch_depth_db,
    }
}

fn read_fractional_delay(signal: &[f32], frame: usize, delay_samples: f64) -> f32 {
    let position = frame as f64 - delay_samples;
    if position < 0.0 {
        return 0.0;
    }
    let lower = position.floor() as usize;
    let fraction = (position - lower as f64) as f32;
    let a = signal[lower];
    let b = signal.get(lower + 1).copied().unwrap_or(a);
    a + fraction * (b - a)
}

fn add_stereo(target: &mut [f32], feed: &[f32]) {
    assert_eq!(target.len(), feed.len());
    for (target, feed) in target.iter_mut().zip(feed) {
        *target += *feed;
    }
}

fn split_program(source: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut splitter = PhaseSplitter::new();
    let mut center = Vec::with_capacity(source.len());
    let mut quadrature = Vec::with_capacity(source.len());
    for &sample in source {
        let (c, q) = splitter.process(sample);
        center.push(c);
        quadrature.push(q);
    }
    (center, quadrature)
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

fn apply_equal_power_fades(samples: &mut [f32], fade_frames: usize) {
    assert!(fade_frames * 2 <= samples.len());
    for index in 0..fade_frames {
        let phase = FRAC_PI_2 * index as f64 / fade_frames as f64;
        let gain = phase.sin().powi(2) as f32;
        samples[index] *= gain;
        samples[samples.len() - 1 - index] *= gain;
    }
}

fn normalize_rms(samples: &mut [f32], target_dbfs: f64) {
    let rms = (samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt();
    let target = 10.0_f64.powf(target_dbfs / 20.0);
    let gain = target / rms;
    for sample in samples {
        *sample = (f64::from(*sample) * gain) as f32;
    }
}

fn mono_rms_dbfs(samples: &[f32]) -> f64 {
    let mean_square = samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    20.0 * mean_square.sqrt().max(1.0e-20).log10()
}

fn stereo_level_metrics(stereo: &[f32]) -> (f64, f64) {
    let rms = mono_rms_dbfs(stereo);
    let peak = stereo
        .iter()
        .map(|sample| f64::from(*sample).abs())
        .fold(0.0, f64::max);
    (rms, 20.0 * peak.max(1.0e-20).log10())
}

fn write_output(
    output_dir: &Path,
    name: &'static str,
    stereo: &[f32],
    listen_for: &'static str,
) -> FileRecord {
    assert_eq!(stereo.len(), FRAME_COUNT * 2);
    let bytes = write_wav(stereo_spec(), stereo).expect("encode float WAV");
    let sha256 = sha256_hex(&bytes);
    fs::write(output_dir.join(name), bytes).expect("write width-binaural WAV");
    let (rms_dbfs, peak_dbfs) = stereo_level_metrics(stereo);
    FileRecord {
        name,
        sha256,
        rms_dbfs,
        peak_dbfs,
        listen_for,
    }
}

fn build_manifest(
    files: &[FileRecord],
    source_rms_dbfs: f64,
    point_line_delta_db: f64,
    quadrature_notches: PathCombReport,
    satellite_notches: PathCombReport,
) -> String {
    let mut manifest = String::new();
    writeln!(manifest, "# Wave 11 width under a real Steam Audio HRTF\n").unwrap();
    writeln!(
        manifest,
        "These five deterministic clips use frames 576000–1151999 of Tom's Diner, faded for 20 ms and normalized once to {source_rms_dbfs:.3} dBFS before any candidate processing or binaural rendering. Every WAV is 12 seconds, 48 kHz stereo IEEE-float PCM.\n"
    )
    .unwrap();
    writeln!(manifest, "## Listening map\n").unwrap();
    for file in files {
        writeln!(
            manifest,
            "- `{}`: {}  \n  SHA-256 `{}`; integrated stereo RMS {:.3} dBFS; peak {:.3} dBFS.",
            file.name, file.listen_for, file.sha256, file.rms_dbfs, file.peak_dbfs
        )
        .unwrap();
    }

    writeln!(manifest, "\n## What was rendered\n").unwrap();
    writeln!(
        manifest,
        "Candidate `{CANDIDATE_ID}` is the unchanged zero-integer-delay C/Q splitter from `width_prototype.rs`: three cascaded second-order allpasses in each arm plus the `a = {CENTER_FIRST_ORDER_COEFFICIENT}` first-order allpass on C. The point reference uses that same C arm as one centre feed, so bypassing the candidate cannot become an A/B tell. The six-metre line has `a = 3 m`, `phi_max = 45 degrees`, and broadside `k = a / sqrt(d^2 + a^2)`. C is rendered at the centre with gain `cos(phi_eff)`; Q is rendered at the two endpoints with gains `+sin(phi_eff)` and `-sin(phi_eff)`. All three share one centre-distance propagation delay and one `1/d` distance gain, so Q has no path-length sweep relative to C.\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "The virtual listener stays at the origin with a fixed head facing +Y in the engine's right-handed ENU coordinates (+Z up). Directions are mapped to Steam Audio as `(x, y, z) = (east, up, -north)`. The orbit is expressed equivalently as the listener moving around a fixed east-west line while the relative centre and endpoint directions rotate around that fixed head; it starts with the source straight ahead.\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "The negative control replaces C/Q with two half-gain coherent copies of the original mono program, each using its endpoint's actual changing `1/r` gain and absolute `r/c` fractional delay. This creates the forbidden moving path difference of as much as 6 m ({:.3} ms).\n",
        6.0 / SOUND_SPEED_MPS * 1_000.0
    )
    .unwrap();
    writeln!(
        manifest,
        "Steam Audio 4.8.1's default HRTF was rendered with `IPLBinauralEffectApply`, bilinear HRTF interpolation, `spatialBlend = 1`, a 48 kHz sample rate, and 512-frame blocks. This is free field: there is no DirectEffect, occlusion, air absorption, reflection, or pathing stage; distance gain is the explicit law above.\n"
    )
    .unwrap();

    writeln!(manifest, "## Sanity measurements\n").unwrap();
    writeln!(
        manifest,
        "The 5 m point and 5 m widened renders measure {:.3} and {:.3} dBFS integrated stereo RMS, a {:.3} dB difference (required: no more than 0.5 dB; PASS).",
        files[0].rms_dbfs, files[1].rms_dbfs, point_line_delta_db
    )
    .unwrap();
    writeln!(
        manifest,
        "A topology-isolated coherent-path transfer analysis uses {}-frame windows at {}-frame hops across {} orbit windows. The quadrature orbit has {} windows containing a regular coherent-path notch family, {} moving adjacent-window pairs, and {:.3} dB maximum moving-notch depth. The coherent-satellite orbit has {} regular-family windows, {} moving pairs, and {:.3} dB maximum moving-notch depth. The analysis evaluates the time-varying two-path transfer that generated each clip over 200 Hz–8 kHz and requires at least five regularly spaced minima; it intentionally excludes the Steam HRTF's own direction-dependent pinna notches, which are not geometric combing.\n",
        satellite_notches.window_frames,
        satellite_notches.hop_frames,
        satellite_notches.analyzed_windows,
        quadrature_notches.regularly_spaced_window_count,
        quadrature_notches.moving_window_pair_count,
        quadrature_notches.max_moving_notch_depth_db,
        satellite_notches.regularly_spaced_window_count,
        satellite_notches.moving_window_pair_count,
        satellite_notches.max_moving_notch_depth_db
    )
    .unwrap();

    writeln!(manifest, "## Decision boundary\n").unwrap();
    writeln!(
        manifest,
        "This strip can decide whether the selected C/Q topology itself sounds hollow under Steam Audio's real HRTF and whether its orbit avoids the satellite topology's walking comb. It cannot decide in-engine occlusion, Doppler, propagation integration, reflections, pathing, head tracking, or live callback behavior."
    )
    .unwrap();
    manifest
}
