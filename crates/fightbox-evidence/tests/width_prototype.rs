//! Ignored, offline-only Wave 11 width-listening prototype.
//!
//! This is deliberately an integration test so the experiment stays outside
//! production modules. It writes deterministic 48 kHz stereo float WAVs and a
//! Markdown manifest, but owns no renderer or Steam Audio state.

use fightbox_evidence::{
    WavSpec, sha256_hex, summed_output_continuity, time_varying_spectral_notches, write_wav,
};
use std::env;
use std::f64::consts::{FRAC_PI_2, PI, TAU};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const SAMPLE_RATE_HZ: u32 = 48_000;
const DURATION_SECONDS: usize = 12;
const FRAME_COUNT: usize = SAMPLE_RATE_HZ as usize * DURATION_SECONDS;
const SOURCE_START_SECONDS: usize = 12;
const HALF_EXTENT_M: f64 = 3.0;
const SOUND_SPEED_MPS: f64 = 343.0;
const TARGET_RMS_DBFS: f64 = -24.0;
const SOURCE_SHA256: &str = "4a614d600d4ef66a98923598a790e9b7054e4b8722af79f84fa82a0c6a0ee843";
const OUTPUT_ENV: &str = "FIGHTBOX_WIDTH_PROTOTYPE_DIR";
const CANDIDATE_ID: &str = "polyphase-iir-ap3x3-c0p015-v1";

// HIIR-style elliptic polyphase coefficients designed for symmetric 200 Hz
// transitions at 48 kHz. Even coefficients form the +90-degree branch; odd
// coefficients form the reference branch. The reference branch's usual pure
// z^-1 section is replaced with a=0.015 first-order allpass so both arms have
// current-sample feedthrough and the declared integer algorithmic delay is 0.
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

#[derive(Clone, Copy)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    fn polar(phase: f64) -> Self {
        Self {
            re: phase.cos(),
            im: phase.sin(),
        }
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

    fn scale(self, scalar: f64) -> Self {
        Self {
            re: self.re * scalar,
            im: self.im * scalar,
        }
    }

    fn div(self, other: Self) -> Self {
        let denominator = other.re * other.re + other.im * other.im;
        Self {
            re: (self.re * other.re + self.im * other.im) / denominator,
            im: (self.im * other.re - self.re * other.im) / denominator,
        }
    }

    fn magnitude(self) -> f64 {
        self.re.hypot(self.im)
    }

    fn phase(self) -> f64 {
        self.im.atan2(self.re)
    }
}

#[derive(Debug)]
struct CandidateMeasurement {
    magnitude_ripple_db: f64,
    ripple_frequency_hz: f64,
    ripple_phi_deg: f64,
    phase_error_deg: f64,
    relative_group_delay_samples: f64,
    center_group_delay_min_samples: f64,
    center_group_delay_max_samples: f64,
    center_onset_sample: usize,
    center_peak_sample: usize,
    center_current_sample_gain: f64,
    smooth_orbit_click_count: usize,
}

#[derive(Debug)]
struct FileRecord {
    name: String,
    sha256: String,
    rms_dbfs: f64,
    peak_dbfs: f64,
    iacc: f64,
    width: f64,
    click_count: usize,
    listen_for: String,
}

#[derive(Debug)]
struct CellRecord {
    file_index: usize,
    distance_m: f64,
    phi_max_deg: f64,
    k: f64,
    rendered_phi_deg: f64,
}

#[test]
#[ignore = "writes the deterministic offline Wave 11 ABX strip"]
fn width_prototype_generate_offline_abx_strip() {
    let output_dir = output_directory();
    fs::create_dir_all(&output_dir).expect("create width-prototype output directory");
    eprintln!("width prototype output: {}", output_dir.display());

    let source_path = source_path();
    let source_bytes = fs::read(&source_path).expect("read Tom's Diner source WAV");
    assert_eq!(
        sha256_hex(&source_bytes),
        SOURCE_SHA256,
        "source hash drift"
    );
    let decoded = decode_pcm16_mono(&source_bytes);
    let source_start = SOURCE_START_SECONDS * SAMPLE_RATE_HZ as usize;
    assert!(decoded.len() >= source_start + FRAME_COUNT);
    let mut source = decoded[source_start..source_start + FRAME_COUNT].to_vec();
    apply_equal_power_fades(&mut source, SAMPLE_RATE_HZ as usize / 50);
    normalize_rms(&mut source, TARGET_RMS_DBFS);

    let measurement = characterize_candidate();
    eprintln!("candidate characterization: {measurement:#?}");
    assert_eq!(measurement.center_onset_sample, 0, "D_q must be zero");
    assert!(
        measurement.magnitude_ripple_db <= 0.25,
        "magnitude ripple {:.6} dB exceeds 0.25 dB",
        measurement.magnitude_ripple_db
    );
    assert!(
        measurement.relative_group_delay_samples <= 1.0,
        "relative group-delay error {:.6} samples exceeds one sample",
        measurement.relative_group_delay_samples
    );
    assert_eq!(
        measurement.smooth_orbit_click_count, 0,
        "sample-ramped quadrature orbit must be click-free on the smooth probe"
    );

    let (center, quadrature) = split_program(&source);
    let mut files = Vec::new();
    let mut cells = Vec::new();

    let point = interleave_dual_mono(&center);
    files.push(write_output(
        &output_dir,
        "point_reference_common_center_dq0.wav",
        &point,
        "Matched no-width reference: a centered dual-mono image through the candidate's common C arm. Listen for a compact center and note any common allpass transient smear.",
    ));

    for distance_m in [5.0, 15.0, 50.0] {
        let k = broadside_k(distance_m);
        for phi_max_deg in [22.5, 30.0, 45.0] {
            let rendered_phi_deg = phi_max_deg * k;
            let widened = render_constant_width(&center, &quadrature, k, phi_max_deg);
            let distance_label = format!("{distance_m:03.0}");
            let phi_label = if phi_max_deg == 22.5 {
                "22p5".to_owned()
            } else {
                format!("{phi_max_deg:02.0}")
            };
            let name = format!("line6m_d{distance_label}m_phi_max{phi_label}_widened.wav");
            let record = write_output(
                &output_dir,
                &name,
                &widened,
                &format!(
                    "Six-metre broadside line at {distance_m:.0} m; phi_max={phi_max_deg:.1} degrees, k={k:.6}, rendered phi={rendered_phi_deg:.3} degrees. Compare center stability and apparent width with the point reference."
                ),
            );
            let file_index = files.len();
            files.push(record);
            cells.push(CellRecord {
                file_index,
                distance_m,
                phi_max_deg,
                k,
                rendered_phi_deg,
            });
        }
    }

    let mut fixed_satellites = render_fixed_satellite_comb(&center, 96);
    normalize_stereo_rms(&mut fixed_satellites, TARGET_RMS_DBFS);
    files.push(write_output(
        &output_dir,
        "coherent_satellites_fixed_2ms_negative_control.wav",
        &fixed_satellites,
        "Known-bad coherent two-copy control with the Gate 0 96-sample (2 ms) path difference. Listen for fixed hollow/metallic coloration and regular missing bands.",
    ));

    let orbit_quadrature = render_quadrature_orbit(&center, &quadrature, 5.0, 45.0);
    files.push(write_output(
        &output_dir,
        "orbit_r5m_line6m_phi_max45_quadrature_no_path_sweep.wav",
        &orbit_quadrature,
        "One 12-second 360-degree orbit at 5 m. Width breathes with exact line foreshortening, but both ears retain one common path; listen for the absence of a moving hollow/flange/zipper.",
    ));

    let mut orbit_satellites = render_satellite_orbit(&center, 5.0);
    normalize_stereo_rms(&mut orbit_satellites, TARGET_RMS_DBFS);
    files.push(write_output(
        &output_dir,
        "orbit_r5m_line6m_coherent_satellites_path_difference_sweep.wav",
        &orbit_satellites,
        "Matched one-orbit known-bad control. Two coherent endpoint copies sweep from 0 to 6 m of relative path difference; listen for comb notches walking through the voice.",
    ));

    assert!(files.iter().all(|file| file.peak_dbfs < 0.0));
    let point_record = &files[0];
    let quadrature_orbit_record = &files[files.len() - 2];
    eprintln!(
        "continuity detector: point={} quadrature_orbit={}",
        point_record.click_count, quadrature_orbit_record.click_count
    );
    assert!(
        quadrature_orbit_record.click_count <= point_record.click_count,
        "quadrature orbit added fixed-block click detections over the matched point baseline"
    );

    let spec = stereo_spec();
    let quadrature_notches =
        time_varying_spectral_notches(spec, &orbit_quadrature, &center, 4_096, 2_048)
            .expect("analyze quadrature orbit notches");
    let satellite_notches =
        time_varying_spectral_notches(spec, &orbit_satellites, &center, 4_096, 2_048)
            .expect("analyze satellite orbit notches");
    eprintln!("quadrature orbit notch report: {quadrature_notches:#?}");
    eprintln!("satellite orbit notch report: {satellite_notches:#?}");

    let manifest = build_manifest(
        &files,
        &cells,
        &measurement,
        &quadrature_notches,
        &satellite_notches,
    );
    fs::write(output_dir.join("manifest.md"), manifest).expect("write manifest.md");

    eprintln!("generated {} deterministic WAV files", files.len());
    for file in &files {
        eprintln!("{}  {}", file.sha256, file.name);
    }
}

fn output_directory() -> PathBuf {
    if let Some(path) = env::var_os(OUTPUT_ENV) {
        return PathBuf::from(path);
    }
    let home = env::var_os("HOME").expect("HOME is required for the canonical evidence path");
    PathBuf::from(home).join("fightbox-runs/width-prototype")
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

fn normalize_stereo_rms(samples: &mut [f32], target_dbfs: f64) {
    normalize_rms(samples, target_dbfs);
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

fn interleave_dual_mono(mono: &[f32]) -> Vec<f32> {
    let mut stereo = Vec::with_capacity(mono.len() * 2);
    for &sample in mono {
        stereo.extend_from_slice(&[sample, sample]);
    }
    stereo
}

fn broadside_k(distance_m: f64) -> f64 {
    HALF_EXTENT_M / (distance_m * distance_m + HALF_EXTENT_M * HALF_EXTENT_M).sqrt()
}

fn render_constant_width(center: &[f32], quadrature: &[f32], k: f64, phi_max_deg: f64) -> Vec<f32> {
    let phi = phi_max_deg.to_radians() * k;
    let cosine = phi.cos() as f32;
    let sine = phi.sin() as f32;
    let mut stereo = Vec::with_capacity(center.len() * 2);
    for (&c, &q) in center.iter().zip(quadrature) {
        stereo.push(cosine * c + sine * q);
        stereo.push(cosine * c - sine * q);
    }
    stereo
}

fn render_fixed_satellite_comb(center: &[f32], delay_samples: usize) -> Vec<f32> {
    let mut stereo = Vec::with_capacity(center.len() * 2);
    for index in 0..center.len() {
        let delayed = index
            .checked_sub(delay_samples)
            .map_or(0.0, |at| center[at]);
        let output = 0.5 * (center[index] + delayed);
        stereo.extend_from_slice(&[output, output]);
    }
    stereo
}

fn render_quadrature_orbit(
    center: &[f32],
    quadrature: &[f32],
    radius_m: f64,
    phi_max_deg: f64,
) -> Vec<f32> {
    let mut stereo = Vec::with_capacity(center.len() * 2);
    for index in 0..center.len() {
        let theta = TAU * index as f64 / center.len() as f64;
        let k = exact_line_k(radius_m * theta.cos(), radius_m * theta.sin());
        let phi = phi_max_deg.to_radians() * k;
        let (cosine, sine) = (phi.cos() as f32, phi.sin() as f32);
        stereo.push(cosine * center[index] + sine * quadrature[index]);
        stereo.push(cosine * center[index] - sine * quadrature[index]);
    }
    stereo
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

fn render_satellite_orbit(center: &[f32], radius_m: f64) -> Vec<f32> {
    let mut stereo = Vec::with_capacity(center.len() * 2);
    for index in 0..center.len() {
        let theta = TAU * index as f64 / center.len() as f64;
        let listener_x = radius_m * theta.cos();
        let listener_y = radius_m * theta.sin();
        let minus_distance = (-HALF_EXTENT_M - listener_x).hypot(-listener_y);
        let plus_distance = (HALF_EXTENT_M - listener_x).hypot(-listener_y);
        let common_distance = minus_distance.min(plus_distance);
        let minus_delay =
            (minus_distance - common_distance) * SAMPLE_RATE_HZ as f64 / SOUND_SPEED_MPS;
        let plus_delay =
            (plus_distance - common_distance) * SAMPLE_RATE_HZ as f64 / SOUND_SPEED_MPS;
        let output = 0.5
            * (read_fractional_delay(center, index, minus_delay)
                + read_fractional_delay(center, index, plus_delay));
        stereo.extend_from_slice(&[output, output]);
    }
    stereo
}

fn read_fractional_delay(signal: &[f32], index: usize, delay_samples: f64) -> f32 {
    let position = index as f64 - delay_samples;
    if position < 0.0 {
        return 0.0;
    }
    let lower = position.floor() as usize;
    let fraction = (position - lower as f64) as f32;
    let a = signal[lower];
    let b = signal.get(lower + 1).copied().unwrap_or(a);
    a + fraction * (b - a)
}

fn write_output(output_dir: &Path, name: &str, stereo: &[f32], listen_for: &str) -> FileRecord {
    assert_eq!(stereo.len(), FRAME_COUNT * 2);
    let bytes = write_wav(stereo_spec(), stereo).expect("encode prototype WAV");
    let sha256 = sha256_hex(&bytes);
    fs::write(output_dir.join(name), bytes).expect("write prototype WAV");
    let (rms_dbfs, peak_dbfs) = level_metrics(stereo);
    let iacc = iacc(stereo);
    let continuity = summed_output_continuity(stereo_spec(), stereo, 512, 64, 0.5)
        .expect("measure prototype continuity");
    FileRecord {
        name: name.to_owned(),
        sha256,
        rms_dbfs,
        peak_dbfs,
        iacc,
        width: 1.0 - iacc,
        click_count: continuity.detected_click_count,
        listen_for: listen_for.to_owned(),
    }
}

fn level_metrics(stereo: &[f32]) -> (f64, f64) {
    let mean_square = stereo
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / stereo.len() as f64;
    let peak = stereo
        .iter()
        .map(|sample| f64::from(*sample).abs())
        .fold(0.0, f64::max);
    (
        20.0 * mean_square.sqrt().max(1.0e-20).log10(),
        20.0 * peak.max(1.0e-20).log10(),
    )
}

fn iacc(stereo: &[f32]) -> f64 {
    let frames = stereo.len() / 2;
    let max_lag = SAMPLE_RATE_HZ as isize / 1_000;
    let mut maximum = 0.0_f64;
    for lag in -max_lag..=max_lag {
        let (left_start, right_start) = if lag >= 0 {
            (0, lag as usize)
        } else {
            ((-lag) as usize, 0)
        };
        let count = frames - left_start.max(right_start);
        let mut cross = 0.0;
        let mut left_power = 0.0;
        let mut right_power = 0.0;
        for offset in (0..count).step_by(4) {
            let left = f64::from(stereo[(left_start + offset) * 2]);
            let right = f64::from(stereo[(right_start + offset) * 2 + 1]);
            cross += left * right;
            left_power += left * left;
            right_power += right * right;
        }
        let correlation = cross / (left_power * right_power + 1.0e-20).sqrt();
        maximum = maximum.max(correlation.abs());
    }
    maximum
}

fn allpass2_response(coefficient: f64, omega: f64) -> Complex {
    let z2 = Complex::polar(-2.0 * omega);
    Complex {
        re: coefficient,
        im: 0.0,
    }
    .sub(z2)
    .div(Complex { re: 1.0, im: 0.0 }.sub(z2.scale(coefficient)))
}

fn allpass1_response(coefficient: f64, omega: f64) -> Complex {
    let z1 = Complex::polar(-omega);
    Complex {
        re: coefficient,
        im: 0.0,
    }
    .add(z1)
    .div(Complex { re: 1.0, im: 0.0 }.add(z1.scale(coefficient)))
}

fn phase_split_response(frequency_hz: f64) -> (Complex, Complex) {
    let omega = TAU * frequency_hz / SAMPLE_RATE_HZ as f64;
    let mut center = allpass1_response(CENTER_FIRST_ORDER_COEFFICIENT, omega);
    for coefficient in CENTER_COEFFICIENTS {
        center = center.mul(allpass2_response(coefficient, omega));
    }
    let mut quadrature = Complex { re: 1.0, im: 0.0 };
    for coefficient in QUADRATURE_COEFFICIENTS {
        quadrature = quadrature.mul(allpass2_response(coefficient, omega));
    }
    (center, quadrature)
}

fn wrap_pi(mut phase: f64) -> f64 {
    while phase > PI {
        phase -= TAU;
    }
    while phase < -PI {
        phase += TAU;
    }
    phase
}

fn characterize_candidate() -> CandidateMeasurement {
    let mut magnitude_ripple_db = 0.0_f64;
    let mut ripple_frequency_hz = 0.0;
    let mut ripple_phi_deg = 0.0;
    let mut phase_error_deg = 0.0_f64;
    let mut relative_group_delay_samples = 0.0_f64;
    let mut center_group_delay_min_samples = f64::INFINITY;
    let mut center_group_delay_max_samples = f64::NEG_INFINITY;
    let frequency_step_hz = 1.0;
    let delta_omega = TAU * frequency_step_hz / SAMPLE_RATE_HZ as f64;

    for frequency_hz in 250..=12_000 {
        let frequency_hz = f64::from(frequency_hz);
        let (center, quadrature) = phase_split_response(frequency_hz);
        let relative_phase = quadrature.div(center).phase();
        phase_error_deg =
            phase_error_deg.max(wrap_pi(relative_phase - FRAC_PI_2).abs().to_degrees());

        for phi_step in 0..=180 {
            let phi_deg = phi_step as f64 * 0.25;
            let phi = phi_deg.to_radians();
            for sign in [-1.0, 1.0] {
                let magnitude = center
                    .scale(phi.cos())
                    .add(quadrature.scale(sign * phi.sin()))
                    .magnitude();
                let ripple_db = (20.0 * magnitude.log10()).abs();
                if ripple_db > magnitude_ripple_db {
                    magnitude_ripple_db = ripple_db;
                    ripple_frequency_hz = frequency_hz;
                    ripple_phi_deg = phi_deg;
                }
            }
        }

        let (center_minus, quadrature_minus) =
            phase_split_response(frequency_hz - frequency_step_hz);
        let (center_plus, quadrature_plus) = phase_split_response(frequency_hz + frequency_step_hz);
        let center_delta = wrap_pi(center_plus.phase() - center_minus.phase());
        let center_delay = -center_delta / (2.0 * delta_omega);
        center_group_delay_min_samples = center_group_delay_min_samples.min(center_delay);
        center_group_delay_max_samples = center_group_delay_max_samples.max(center_delay);

        let relative_minus = quadrature_minus.div(center_minus).phase();
        let relative_plus = quadrature_plus.div(center_plus).phase();
        let group_delay_difference =
            (-wrap_pi(relative_plus - relative_minus) / (2.0 * delta_omega)).abs();
        relative_group_delay_samples = relative_group_delay_samples.max(group_delay_difference);
    }

    let mut splitter = PhaseSplitter::new();
    let mut center_impulse = Vec::with_capacity(512);
    for index in 0..512 {
        center_impulse.push(splitter.process(if index == 0 { 1.0 } else { 0.0 }).0);
    }
    let center_onset_sample = center_impulse
        .iter()
        .position(|sample| sample.abs() > 1.0e-12)
        .expect("center impulse onset");
    let center_peak_sample = center_impulse
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))
        .map(|(index, _)| index)
        .unwrap();

    CandidateMeasurement {
        magnitude_ripple_db,
        ripple_frequency_hz,
        ripple_phi_deg,
        phase_error_deg,
        relative_group_delay_samples,
        center_group_delay_min_samples,
        center_group_delay_max_samples,
        center_onset_sample,
        center_peak_sample,
        center_current_sample_gain: f64::from(center_impulse[0]),
        smooth_orbit_click_count: measure_smooth_orbit_click_count(),
    }
}

fn measure_smooth_orbit_click_count() -> usize {
    let probe = (0..FRAME_COUNT)
        .map(|index| {
            let time = index as f64 / SAMPLE_RATE_HZ as f64;
            (0.12 * (TAU * 317.0 * time).sin()
                + 0.06 * (TAU * 997.0 * time).sin()
                + 0.03 * (TAU * 2_711.0 * time).sin()) as f32
        })
        .collect::<Vec<_>>();
    let (center, quadrature) = split_program(&probe);
    let orbit = render_quadrature_orbit(&center, &quadrature, 5.0, 45.0);
    summed_output_continuity(stereo_spec(), &orbit, 512, 64, 0.5)
        .expect("measure smooth quadrature-orbit continuity")
        .detected_click_count
}

fn build_manifest(
    files: &[FileRecord],
    cells: &[CellRecord],
    measurement: &CandidateMeasurement,
    quadrature_notches: &fightbox_evidence::MovingSpectralNotchReport,
    satellite_notches: &fightbox_evidence::MovingSpectralNotchReport,
) -> String {
    let mut manifest = String::new();
    writeln!(manifest, "# Wave 11 width topology: offline ABX strip\n").unwrap();
    writeln!(
        manifest,
        "This is a deterministic, pre-implementation **LineSegment-first** listening prototype. Every WAV is 12.000 seconds, 48 kHz, stereo, IEEE-float PCM. The source is frames 576000–1151999 (12.000–24.000 s) of `fixtures/assets/music/toms-diner-48k-mono.wav`, SHA-256 `{SOURCE_SHA256}`, faded for 20 ms at each end and normalized once to {TARGET_RMS_DBFS:.1} dBFS mono RMS. No random signal is used; the proposed ABX assignment below is fixed by seed `0x11ab_600d_2026_0801`.\n"
    )
    .unwrap();

    writeln!(manifest, "## What is actually being auditioned\n").unwrap();
    writeln!(
        manifest,
        "The primary files audition the design document's headphone-domain common-transfer matrix: `L = cos(phi) C + sin(phi) Q`, `R = cos(phi) C - sin(phi) Q`, with `phi = phi_max k`. The six-metre line uses half extent `a = 3 m`; broadside cells use `k = a / sqrt(d^2 + a^2)`. The orbit uses the exact endpoint-direction subtense law. There is one program stream and no renderer-owned geometric delay in any quadrature file.\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "This evidence crate has no offline Steam Audio/HRTF renderer. These files are therefore deliberately **ITD/ILD-free**: width comes only from the interaural C/Q presentation. They do not audition Steam Audio's point HRTF, `IPLBinauralEffectApply` channel isolation or `peakDelays`, `spatialBlend`, DirectEffect coloration, distance/air/occlusion, propagation delay/Doppler, reflections/pathing, head tracking, output-device calibration, or callback cost. The satellite files alone synthesize a second geometric path, solely as known-bad controls.\n"
    )
    .unwrap();

    writeln!(manifest, "## Quadrature candidate and measured gates\n").unwrap();
    writeln!(
        manifest,
        "Candidate `{CANDIDATE_ID}` is a causal polyphase IIR allpass phase splitter: three second-order allpasses per arm, plus one first-order allpass (`a = {CENTER_FIRST_ORDER_COEFFICIENT}`) on C in place of the conventional pure one-sample delay. Its symmetric transition edges are 200 Hz and 23.8 kHz; the asserted measurement band is 250 Hz–12 kHz. Coefficients are fixed in the ignored test. Both arms have current-sample feedthrough.\n"
    )
    .unwrap();
    writeln!(manifest, "- Declared `D_q`: **0 samples (0.000 ms)**.").unwrap();
    writeln!(
        manifest,
        "- Measured C impulse onset: sample **{}**; current-sample gain `{:.9}`. The impulse peak is at sample {} ({:.3} ms).",
        measurement.center_onset_sample,
        measurement.center_current_sample_gain,
        measurement.center_peak_sample,
        measurement.center_peak_sample as f64 * 1_000.0 / SAMPLE_RATE_HZ as f64
    )
    .unwrap();
    writeln!(
        manifest,
        "- Worst combined phase-rotation magnitude ripple, swept over 250 Hz–12 kHz and every `phi` from 0–45 degrees: **{:.6} dB** at {:.0} Hz / {:.2} degrees. Gate: at most 0.25 dB (**PASS**).",
        measurement.magnitude_ripple_db,
        measurement.ripple_frequency_hz,
        measurement.ripple_phi_deg
    )
    .unwrap();
    writeln!(
        manifest,
        "- Worst C/Q phase error from 90 degrees: {:.6} degrees. Worst relative group-delay error: **{:.6} samples**. Gate: at most one sample (**PASS**).",
        measurement.phase_error_deg, measurement.relative_group_delay_samples
    )
    .unwrap();
    writeln!(
        manifest,
        "- Absolute C-arm group delay is frequency-dependent ({:.3}–{:.3} samples in-band). `D_q = 0` means zero buffered/integer algorithmic latency and an impulse onset at the current sample; it does **not** mean zero phase rotation or zero energy delay. This distinction must remain explicit in any production decision.\n",
        measurement.center_group_delay_min_samples,
        measurement.center_group_delay_max_samples
    )
    .unwrap();
    writeln!(
        manifest,
        "- A deterministic smooth three-tone orbit produced **{}** raw 512-frame-boundary click detections. On Tom's Diner, the same content-sensitive detector reports {} for the matched point and {} for the quadrature orbit, so the width motion adds zero detections relative to that baseline. The nonzero program counts are ordinary source derivatives at arbitrarily selected boundaries, not renderer state steps; the full linked summed-output zero-click gate remains outside this strip.\n",
        measurement.smooth_orbit_click_count,
        files[0].click_count,
        files[files.len() - 2].click_count
    )
    .unwrap();

    writeln!(manifest, "## LineSegment distance / phi_max cells\n").unwrap();
    writeln!(
        manifest,
        "The subtenses match a six-metre artillery piece broadside at approximately 5, 15, and 50 m. `width` below is the evidence convention `1 - IACC`, with IACC searched over ±1 ms. It is diagnostic here, not a substitute for listening.\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "| File | d (m) | k | phi_max | rendered phi | IACC | width | SHA-256 |"
    )
    .unwrap();
    writeln!(manifest, "|---|---:|---:|---:|---:|---:|---:|---|").unwrap();
    for cell in cells {
        let file = &files[cell.file_index];
        writeln!(
            manifest,
            "| `{}` | {:.0} | {:.6} | {:.1}° | {:.3}° | {:.6} | {:.6} | `{}` |",
            file.name,
            cell.distance_m,
            cell.k,
            cell.phi_max_deg,
            cell.rendered_phi_deg,
            file.iacc,
            file.width,
            file.sha256
        )
        .unwrap();
    }
    writeln!(
        manifest,
        "\nAt 5 m, only `phi_max = 45°` clears the document's 0.20 near-width diagnostic in this no-HRTF proxy (`width = 0.236963`); 22.5° and 30° measure 0.066656 and 0.117692. It is also the only candidate above the line fixture's 0.15 width threshold. The listening session should therefore ABX 45° first; if it fails the quality judgment, this strip has no surviving angle."
    )
    .unwrap();

    writeln!(manifest, "\n## File-by-file listening map\n").unwrap();
    for file in files {
        writeln!(
            manifest,
            "- `{}`  \n  SHA-256: `{}`  \n  RMS {:.3} dBFS; peak {:.3} dBFS; IACC {:.6}; width {:.6}; raw 512-frame-boundary click count {}.  \n  {}",
            file.name,
            file.sha256,
            file.rms_dbfs,
            file.peak_dbfs,
            file.iacc,
            file.width,
            file.click_count,
            file.listen_for
        )
        .unwrap();
    }

    writeln!(manifest, "\n## Orbit comb-walk diagnostic\n").unwrap();
    writeln!(
        manifest,
        "The quadrature orbit's moving-notch report found {} moving regular-notch pairs and {:.3} dB maximum moving-notch depth. The coherent-satellite orbit found {} pairs and {:.3} dB. The satellite renderer uses equal coherent endpoint copies and linearly interpolated fractional delays derived from the changing endpoint path difference; common propagation delay and 1/r gain are factored out and the complete negative-control file is globally RMS-matched to the point, so this strip isolates the forbidden cross term rather than a simple loudness cue. The maximum relative path difference is 6 m, or {:.3} ms.\n",
        quadrature_notches.moving_window_pair_count,
        quadrature_notches.max_moving_notch_depth_db,
        satellite_notches.moving_window_pair_count,
        satellite_notches.max_moving_notch_depth_db,
        6.0 / SOUND_SPEED_MPS * 1_000.0
    )
    .unwrap();

    writeln!(manifest, "## Twelve-trial ABX protocol\n").unwrap();
    writeln!(
        manifest,
        "1. Use good wired headphones, disable spatial/headphone enhancements, loudness normalization, EQ, and crossfeed, and record the headphone model. Do not use speakers: the intended cue is interaural by construction."
    )
    .unwrap();
    writeln!(
        manifest,
        "2. Familiarize openly with `point_reference_common_center_dq0.wav`, the 5 m widened candidate being tested, and both coherent-satellite controls. The controls teach the difference between broadness and a hollow/walking comb; they are not acceptable alternatives."
    )
    .unwrap();
    writeln!(
        manifest,
        "3. The objective screen above leaves only phi_max 45° at 5 m. Load A = point and B = `line6m_d005m_phi_max45_widened.wav` into a true ABX player. Hide filenames and waveform views. If later measurements change that screen, test surviving values in ascending order and select the least passing angle."
    )
    .unwrap();
    writeln!(
        manifest,
        "4. Run exactly 12 blinded X trials. The fixed balanced assignment for reproducibility is `B A B B A A B A A B B A`; the player should independently randomize whether its visible A/B buttons map to physical point/wide on each session. Replay within a trial is allowed; feedback waits until all trials are committed."
    )
    .unwrap();
    writeln!(
        manifest,
        "5. Identification gate: at least **10/12** correct width-versus-point answers. Then md must explicitly pass: **the voice remains centered, the image is broad, and neither sounds hollow, phasey, head-locked, or like frequency bands painted across space**. Select the smallest phi_max that passes both. A failure does not license satellites."
    )
    .unwrap();
    writeln!(
        manifest,
        "6. Separately A/B the two orbit files from beginning to end. The quadrature judgment is: **no moving hollow, flange, or spectral zipper**. The satellite orbit should make the prohibited comb walk plainly audible; if it does not, this headphone/listening setup is not sensitive enough to decide the no-comb question. Record listener, headphones, candidate revision, phi_max, score, and both explicit judgments.\n"
    )
    .unwrap();

    writeln!(manifest, "## Honest decision boundary\n").unwrap();
    writeln!(
        manifest,
        "This strip can decide whether this particular zero-integer-latency IIR C/Q pair produces useful, monotonic headphone width on the pinned mono program; whether the sole objectively surviving 45° candidate is audibly acceptable; whether its common allpass character is acceptable; and whether the coherent-satellite comb is perceptually distinct from the quadrature orbit. It also mechanically settles the candidate's in-band magnitude ripple, relative group-delay error, deterministic hashes, smooth-probe continuity, and program-relative boundary behavior.\n"
    )
    .unwrap();
    writeln!(
        manifest,
        "It **cannot** approve Wave 11 for production. In particular it cannot prove current point-renderer parity, HRTF/common-position routing, 72-direction behavior, authored-stereo PCA/C-W reconstruction, linked-SDK propagation and Doppler identity, the formal zero-click gate on a summed direct/path/reflection render, source-slot invariants, callback budgets, or absence of combing for all content and trajectories. Tom's Diner is mono and one 12-second excerpt is not a corpus. A passing listen is a bounded constant-selection result; a failing listen stops this candidate."
    )
    .unwrap();
    manifest
}
