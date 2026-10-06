use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fightbox_api::AssetAnalysis;
use fightbox_evidence::{WavSpec, analyze_decoded_asset, multitone, pink_like, sha256_hex, sine};
use serde::Deserialize;

#[cfg(target_os = "macos")]
#[path = "core_audio.rs"]
mod core_audio;

pub const SONG_SAMPLE_RATE_HZ: u32 = 48_000;

#[derive(Debug, Deserialize)]
pub struct AssetDescriptor {
    pub asset_id: String,
    pub kind: AssetKind,
    pub generator: Generator,
    pub channels: u16,
    pub sample_rate_hz: u32,
    pub duration_s: f64,
    #[serde(default)]
    pub onsets_s: Vec<f64>,
    pub target_rms_dbfs: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Sine,
    Multitone,
    PinkLike,
    Wav,
    Song,
}

#[derive(Debug, Deserialize)]
pub struct Generator {
    pub sine: Option<SineBlock>,
    pub multitone: Option<MultitoneBlock>,
    pub pink_like: Option<PinkLikeBlock>,
    pub wav: Option<WavBlock>,
    pub song: Option<SongBlock>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SineBlock {
    pub frequency_hz: f64,
}

#[derive(Debug, Deserialize)]
pub struct MultitoneBlock {
    pub frequencies_hz: Vec<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct PinkLikeBlock {
    pub seed: u64,
}

#[derive(Debug, Deserialize)]
pub struct WavBlock {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub start_frame: u64,
    #[serde(default)]
    pub r#loop: bool,
}

#[derive(Debug, Deserialize)]
pub struct SongBlock {
    pub path: String,
}

pub struct SongProgram {
    pub frames: Vec<[f32; 2]>,
    pub channels: u16,
}

#[derive(Clone)]
pub struct PreparedAsset {
    pub samples: Vec<f32>,
    pub stereo_samples: Option<Vec<[f32; 2]>>,
    pub song_path: Option<PathBuf>,
    pub analysis: AssetAnalysis,
    pub onset_frames: Vec<u32>,
    /// Whether playback wraps after the last prepared sample.
    pub loops: bool,
    /// Hash of the descriptor that declares generation settings or the verified WAV hash.
    pub descriptor_sha256: String,
}

impl PreparedAsset {
    pub fn live_music() -> Self {
        Self {
            samples: vec![0.0],
            stereo_samples: None,
            song_path: None,
            analysis: AssetAnalysis::new(
                -14.0,
                0.0,
                fightbox_api::AssetMeasurementProvenance::new(
                    "live-music/v1: assumed mono program RMS -14 dBFS; nominal true peak 0 dBTP",
                )
                .expect("fixed live calibration provenance"),
            )
            .expect("fixed live calibration levels"),
            onset_frames: Vec::new(),
            loops: true,
            descriptor_sha256: sha256_hex(b"live-music-nominal-minus14-v1"),
        }
    }

    /// Builds runtime echo state only when the fixture independently opts in.
    /// An absent descriptor onset table remains the structural Off profile.
    pub fn echo_profile(
        &self,
        impulsive: bool,
        impulse_class: fightbox_api::ImpulseClass,
    ) -> Result<fightbox_steam_audio::EchoProfile, String> {
        if !impulsive {
            return Ok(fightbox_steam_audio::EchoProfile::OFF);
        }
        let loop_frames = u32::try_from(self.samples.len())
            .map_err(|_| "asset loop is too long for echo onset scheduling".to_owned())?;
        fightbox_steam_audio::EchoProfile::from_loop_frames(
            loop_frames,
            &self.onset_frames,
            impulse_class,
        )
        .map_err(|error| format!("invalid echo profile: {error:?}"))
    }
}

pub fn load_asset(asset_id: &str) -> Result<PreparedAsset, String> {
    let descriptor_started = Instant::now();
    let descriptor_path = repository_root()
        .join("fixtures/assets")
        .join(format!("{asset_id}.json"));
    let bytes = std::fs::read(&descriptor_path)
        .map_err(|error| format!("cannot read {}: {error}", descriptor_path.display()))?;
    let descriptor: AssetDescriptor = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid asset descriptor: {error}"))?;
    if descriptor.asset_id != asset_id
        || (if descriptor.kind == AssetKind::Song {
            !(1..=2).contains(&descriptor.channels)
        } else {
            descriptor.channels != 1
        })
        || descriptor.sample_rate_hz != 48_000
        || !descriptor.duration_s.is_finite()
        || descriptor.duration_s <= 0.0
    {
        return Err(format!("asset descriptor {asset_id} is incompatible"));
    }
    let descriptor_elapsed = descriptor_started.elapsed();
    let frames = (descriptor.duration_s * f64::from(descriptor.sample_rate_hz)).round() as usize;
    let mut onset_frames = Vec::with_capacity(descriptor.onsets_s.len());
    let mut previous_onset = None;
    for (index, onset) in descriptor.onsets_s.iter().copied().enumerate() {
        if !onset.is_finite()
            || onset < 0.0
            || onset >= descriptor.duration_s
            || previous_onset.is_some_and(|previous| onset <= previous)
        {
            return Err(format!(
                "asset descriptor {asset_id} onsets_s[{index}] is not strictly ascending in [0, duration_s)"
            ));
        }
        let frame = (onset * f64::from(descriptor.sample_rate_hz)).round() as usize;
        if frame >= frames
            || frame > u32::MAX as usize
            || onset_frames
                .last()
                .is_some_and(|previous| frame as u32 <= *previous)
        {
            return Err(format!(
                "asset descriptor {asset_id} onsets_s[{index}] does not map to a distinct loop frame"
            ));
        }
        onset_frames.push(frame as u32);
        previous_onset = Some(onset);
    }
    if descriptor.kind == AssetKind::Song {
        if descriptor.target_rms_dbfs != -14.0 {
            return Err("song descriptors use nominal target_rms_dbfs -14".into());
        }
        let song = descriptor
            .generator
            .song
            .ok_or("song generator is missing")?;
        let path = resolve_repository_path(&song.path);
        let mut asset = prepare_song(&path, descriptor.channels == 2)?;
        if onset_frames
            .last()
            .is_some_and(|frame| *frame as usize >= asset.samples.len())
        {
            return Err(format!(
                "asset descriptor {asset_id} onset lies outside the song"
            ));
        }
        asset.onset_frames = onset_frames;
        asset.descriptor_sha256 = sha256_hex(&bytes);
        eprintln!(
            "[startup] asset {asset_id}: descriptor {} ms, song decode+normalize+analysis {} ms",
            descriptor_elapsed.as_millis(),
            descriptor_started
                .elapsed()
                .saturating_sub(descriptor_elapsed)
                .as_millis()
        );
        return Ok(asset);
    }
    let spec = WavSpec {
        sample_rate_hz: descriptor.sample_rate_hz,
        channels: 1,
    };
    let loops = match descriptor.kind {
        AssetKind::Wav => descriptor
            .generator
            .wav
            .as_ref()
            .is_some_and(|wav| wav.r#loop),
        AssetKind::Sine | AssetKind::Multitone | AssetKind::PinkLike | AssetKind::Song => true,
    };
    let prepare_started = Instant::now();
    let (samples, load_timing) = match descriptor.kind {
        AssetKind::Sine => {
            let samples = sine(
                spec,
                descriptor
                    .generator
                    .sine
                    .ok_or("sine generator is missing")?
                    .frequency_hz as f32,
                frames,
                descriptor.target_rms_dbfs as f32,
            )
            .map_err(|error| error.as_str().to_owned())?
            .samples;
            (
                samples,
                AssetLoadTiming::Generated(prepare_started.elapsed()),
            )
        }
        AssetKind::Multitone => {
            let frequencies = descriptor
                .generator
                .multitone
                .ok_or("multitone generator is missing")?
                .frequencies_hz
                .into_iter()
                .map(|frequency| frequency as f32)
                .collect::<Vec<_>>();
            let samples = multitone(
                spec,
                &frequencies,
                frames,
                descriptor.target_rms_dbfs as f32,
            )
            .map_err(|error| error.as_str().to_owned())?
            .samples;
            (
                samples,
                AssetLoadTiming::Generated(prepare_started.elapsed()),
            )
        }
        AssetKind::PinkLike => {
            let samples = pink_like(
                spec,
                descriptor
                    .generator
                    .pink_like
                    .ok_or("pink_like generator is missing")?
                    .seed,
                frames,
                descriptor.target_rms_dbfs as f32,
            )
            .map_err(|error| error.as_str().to_owned())?
            .samples;
            (
                samples,
                AssetLoadTiming::Generated(prepare_started.elapsed()),
            )
        }
        AssetKind::Wav => {
            let (samples, timing) = load_wav(
                descriptor.generator.wav.ok_or("WAV generator is missing")?,
                frames,
                descriptor.target_rms_dbfs as f32,
            )?;
            (samples, AssetLoadTiming::Wav(timing))
        }
        AssetKind::Song => unreachable!("songs prepared before pinned WAV generation"),
    };
    let analysis_started = Instant::now();
    let analysis = analyze_decoded_asset(spec, &samples)
        .map_err(|error| format!("cannot analyze asset {asset_id}: {}", error.as_str()))?
        .into_parts()
        .0;
    let analysis_elapsed = analysis_started.elapsed();
    match load_timing {
        AssetLoadTiming::Generated(generation_elapsed) => eprintln!(
            "[startup] asset {asset_id}: descriptor {} ms, generate+normalize {} ms, analysis {} ms",
            descriptor_elapsed.as_millis(),
            generation_elapsed.as_millis(),
            analysis_elapsed.as_millis()
        ),
        AssetLoadTiming::Wav(timing) => eprintln!(
            "[startup] asset {asset_id}: descriptor {} ms, read {} ms, hash {} ms, \
             decode+normalize {} ms, analysis {} ms",
            descriptor_elapsed.as_millis(),
            timing.read.as_millis(),
            timing.hash.as_millis(),
            timing.decode_and_normalize.as_millis(),
            analysis_elapsed.as_millis()
        ),
    }
    Ok(PreparedAsset {
        samples,
        stereo_samples: None,
        song_path: None,
        analysis,
        onset_frames,
        loops,
        descriptor_sha256: sha256_hex(&bytes),
    })
}

enum AssetLoadTiming {
    Generated(Duration),
    Wav(WavLoadTiming),
}

struct WavLoadTiming {
    read: Duration,
    hash: Duration,
    decode_and_normalize: Duration,
}

fn load_wav(
    wav: WavBlock,
    frames: usize,
    target_rms_dbfs: f32,
) -> Result<(Vec<f32>, WavLoadTiming), String> {
    let path = resolve_repository_path(&wav.path);
    let read_started = Instant::now();
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("cannot read WAV {}: {error}", path.display()))?;
    let read = read_started.elapsed();
    let hash_started = Instant::now();
    let actual_hash = sha256_hex(&bytes);
    let hash = hash_started.elapsed();
    if actual_hash != wav.sha256 {
        return Err(format!("WAV {} sha256 mismatch", path.display()));
    }
    let decode_started = Instant::now();
    let source = decode_mono_wav(&bytes)?;
    let start = usize::try_from(wav.start_frame).map_err(|_| "WAV start frame is too large")?;
    if start >= source.len() {
        return Err("WAV start frame lies outside the source".into());
    }
    let mut samples = Vec::with_capacity(frames);
    for frame in 0..frames {
        let source_frame = start + frame;
        samples.push(if wav.r#loop {
            source[source_frame % source.len()]
        } else {
            source.get(source_frame).copied().unwrap_or(0.0)
        });
    }
    normalize_rms(&mut samples, target_rms_dbfs)?;
    Ok((
        samples,
        WavLoadTiming {
            read,
            hash,
            decode_and_normalize: decode_started.elapsed(),
        },
    ))
}

/// Decode a personal file without copying it into the asset tree.
pub fn decode_song(path: &Path) -> Result<SongProgram, String> {
    if path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("ogg") || extension.eq_ignore_ascii_case("oga")
    }) {
        return Err("Ogg/Vorbis songs are not supported by macOS AudioToolbox".into());
    }
    #[cfg(target_os = "macos")]
    {
        core_audio::decode(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(
            "song file decoding requires macOS AudioToolbox; pinned WAV assets remain supported"
                .into(),
        )
    }
}

/// Match the nominal live-program RMS before the scene's single gain chain.
pub fn prepare_song(path: &Path, stereo: bool) -> Result<PreparedAsset, String> {
    let song = decode_song(path)?;
    let stereo = stereo && song.channels == 2;
    let mut program = if stereo {
        song.frames
            .iter()
            .flat_map(|frame| *frame)
            .collect::<Vec<_>>()
    } else {
        song.frames
            .iter()
            .map(|[left, right]| left * 0.5 + right * 0.5)
            .collect()
    };
    let analysis = normalize_song_program(&mut program, if stereo { 2 } else { 1 })?;
    let (samples, stereo_samples) = if stereo {
        let frames = program
            .chunks_exact(2)
            .map(|frame| [frame[0], frame[1]])
            .collect::<Vec<_>>();
        let mono = frames
            .iter()
            .map(|[left, right]| left * 0.5 + right * 0.5)
            .collect();
        (mono, Some(frames))
    } else {
        (program, None)
    };
    Ok(PreparedAsset {
        samples,
        stereo_samples,
        song_path: Some(path.to_owned()),
        analysis,
        onset_frames: Vec::new(),
        loops: true,
        descriptor_sha256: sha256_hex(path.as_os_str().as_encoded_bytes()),
    })
}

/// Fold a stereo song descriptor for a source without a stereo-image extent.
pub fn fold_song_to_mono(asset: &mut PreparedAsset) -> Result<(), String> {
    if asset.stereo_samples.is_some() {
        asset.analysis = normalize_song_program(&mut asset.samples, 1)?;
        asset.stereo_samples = None;
    }
    Ok(())
}

fn normalize_song_program(program: &mut [f32], channels: u16) -> Result<AssetAnalysis, String> {
    let rms = (program
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / program.len() as f64)
        .sqrt();
    if !rms.is_finite() || rms <= 0.0 {
        return Err("song program is silent after channel fold".into());
    }
    let gain = (10.0_f64.powf(-14.0 / 20.0) / rms) as f32;
    for sample in program.iter_mut() {
        *sample *= gain;
    }
    analyze_decoded_asset(
        WavSpec {
            sample_rate_hz: SONG_SAMPLE_RATE_HZ,
            channels,
        },
        program,
    )
    .map_err(|error| format!("cannot analyze song: {}", error.as_str()))
    .map(|analysis| analysis.into_parts().0)
}

fn decode_mono_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let (sample_rate, channels, samples) = decode_program_wav(bytes)?;
    if channels != 1 || sample_rate != 48_000 {
        return Err("WAV must be mono 48 kHz".into());
    }
    Ok(samples)
}

#[cfg(feature = "live-output")]
pub fn load_live_input_wav(path: &Path) -> Result<(u32, Vec<[f32; 2]>), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("cannot read test WAV {}: {error}", path.display()))?;
    let (rate, _, frames) = decode_program_wav_frames(&bytes)?;
    if frames.is_empty() {
        return Err("test WAV is empty".into());
    }
    Ok((rate, frames))
}

fn decode_program_wav(bytes: &[u8]) -> Result<(u32, u16, Vec<f32>), String> {
    let (sample_rate, channels, frames) = decode_program_wav_frames(bytes)?;
    let mono = frames
        .into_iter()
        .map(|[left, right]| left * 0.5 + right * 0.5)
        .collect();
    Ok((sample_rate, channels, mono))
}

fn decode_program_wav_frames(bytes: &[u8]) -> Result<(u32, u16, Vec<[f32; 2]>), String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("malformed RIFF/WAVE header".into());
    }
    let mut format = None;
    let mut data = None;
    let mut position = 12;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size =
            u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
        position += 8;
        let end = position
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or("truncated WAV chunk")?;
        if id == b"fmt " && size >= 16 {
            let body = &bytes[position..end];
            format = Some((
                u16::from_le_bytes(body[0..2].try_into().unwrap()),
                u16::from_le_bytes(body[2..4].try_into().unwrap()),
                u32::from_le_bytes(body[4..8].try_into().unwrap()),
                u16::from_le_bytes(body[14..16].try_into().unwrap()),
            ));
        } else if id == b"data" {
            data = Some(&bytes[position..end]);
        }
        position = end + (size & 1);
    }
    let (tag, channels, sample_rate, bits) = format.ok_or("WAV fmt chunk is missing")?;
    if !(1..=2).contains(&channels) || !(8_000..=192_000).contains(&sample_rate) {
        return Err("WAV must have one or two channels at 8..192 kHz".into());
    }
    let data = data.ok_or("WAV data chunk is missing")?;
    let sample_bytes = match (tag, bits) {
        (1, 16) => 2,
        (1, 24) => 3,
        (1, 32) | (3, 32) => 4,
        _ => return Err("WAV must be 16/24/32-bit PCM or 32-bit float".into()),
    };
    let frame_bytes = sample_bytes * usize::from(channels);
    if data.len() % frame_bytes != 0 {
        return Err("truncated WAV frame".into());
    }
    let decode = |sample: &[u8]| -> f32 {
        match (tag, bits) {
            (1, 16) => i16::from_le_bytes(sample.try_into().unwrap()) as f32 / 32768.0,
            (1, 24) => {
                ((i32::from(sample[0])
                    | (i32::from(sample[1]) << 8)
                    | (i32::from(sample[2]) << 16))
                    << 8) as f32
                    / 2147483648.0
            }
            (1, 32) => i32::from_le_bytes(sample.try_into().unwrap()) as f32 / 2147483648.0,
            (3, 32) => f32::from_le_bytes(sample.try_into().unwrap()),
            _ => unreachable!(),
        }
    };
    let frames = data
        .chunks_exact(frame_bytes)
        .map(|frame| {
            let left = decode(&frame[..sample_bytes]);
            let right = if channels == 2 {
                decode(&frame[sample_bytes..])
            } else {
                left
            };
            if !left.is_finite() || !right.is_finite() {
                return Err("non-finite WAV sample".to_owned());
            }
            Ok([left, right])
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((sample_rate, channels, frames))
}

fn normalize_rms(samples: &mut [f32], target_rms_dbfs: f32) -> Result<(), String> {
    let rms = (samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt() as f32;
    if rms <= 0.0 {
        return Err("WAV selection is silent".into());
    }
    let gain = 10.0_f32.powf((target_rms_dbfs - 20.0 * rms.log10()) / 20.0);
    if samples.iter().any(|sample| sample.abs() * gain > 1.0) {
        return Err("WAV normalization would clip".into());
    }
    for sample in samples {
        *sample *= gain;
    }
    Ok(())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn resolve_repository_path(value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else {
        repository_root().join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_descriptor_and_unsupported_ogg_are_clear() {
        let descriptor: AssetDescriptor = serde_json::from_value(serde_json::json!({
            "asset_id": "music/personal",
            "kind": "song",
            "generator": { "song": { "path": "/Users/me/Music/song.m4a" } },
            "channels": 2,
            "sample_rate_hz": 48_000,
            "duration_s": 1.0,
            "target_rms_dbfs": -14.0
        }))
        .unwrap();
        assert_eq!(descriptor.kind, AssetKind::Song);
        assert_eq!(
            descriptor.generator.song.unwrap().path,
            "/Users/me/Music/song.m4a"
        );
        assert!(
            decode_song(Path::new("song.OGG"))
                .err()
                .unwrap()
                .contains("Ogg/Vorbis")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn song_aac_decodes_resamples_and_measures_nominal_mono() {
        let directory = SongTestDirectory::new("mono");
        let path = directory.aac(1);
        let song = decode_song(&path).unwrap();
        assert_eq!(SONG_SAMPLE_RATE_HZ, 48_000);
        assert_eq!(song.channels, 1);
        assert!(
            song.frames.len().abs_diff(48_000) <= 1_115,
            "{} frames",
            song.frames.len()
        );
        assert!(song.frames.iter().all(|[left, right]| left == right));
        let mono = song.frames.iter().map(|frame| frame[0]).collect::<Vec<_>>();
        let expected_rms = 20.0 * (0.2_f64 / 2.0_f64.sqrt()).log10();
        assert!((test_rms_dbfs(&mono) - expected_rms).abs() <= 0.5);
        assert!((test_tone_frequency(&mono) - 1_000.0).abs() < 2.0);
        let prepared = prepare_song(&path, true).unwrap();
        assert!(prepared.stereo_samples.is_none());
        assert!(prepared.loops);
        assert!((prepared.analysis.program_rms_dbfs + 14.0).abs() < 0.001);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn song_aac_preserves_stereo_channel_order_and_calibrates_fold() {
        let directory = SongTestDirectory::new("stereo");
        let path = directory.aac(2);
        let song = decode_song(&path).unwrap();
        assert_eq!(song.channels, 2);
        assert!(song.frames.len().abs_diff(48_000) <= 1_115);
        let left = song.frames.iter().map(|frame| frame[0]).collect::<Vec<_>>();
        let right = song.frames.iter().map(|frame| frame[1]).collect::<Vec<_>>();
        assert!((test_tone_frequency(&left) - 1_000.0).abs() < 2.0);
        assert!((test_tone_frequency(&right) - 2_000.0).abs() < 2.0);
        for (program, amplitude) in [(&left, 0.2_f64), (&right, 0.1_f64)] {
            let expected_rms = 20.0 * (amplitude / 2.0_f64.sqrt()).log10();
            assert!((test_rms_dbfs(program) - expected_rms).abs() <= 0.5);
        }
        let mut prepared = prepare_song(&path, true).unwrap();
        assert!(prepared.stereo_samples.is_some());
        assert!((prepared.analysis.program_rms_dbfs + 14.0).abs() < 0.001);
        fold_song_to_mono(&mut prepared).unwrap();
        assert!(prepared.stereo_samples.is_none());
        assert!((prepared.analysis.program_rms_dbfs + 14.0).abs() < 0.001);
        let directly_folded = prepare_song(&path, false).unwrap();
        assert!(
            prepared
                .samples
                .iter()
                .zip(&directly_folded.samples)
                .all(|(a, b)| (a - b).abs() < 0.000_001)
        );
    }

    #[cfg(target_os = "macos")]
    struct SongTestDirectory(PathBuf);

    #[cfg(target_os = "macos")]
    impl SongTestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "fightbox-song-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn aac(&self, channels: u16) -> PathBuf {
            let frames = 44_100;
            let data_size = frames * u32::from(channels) * 2;
            let mut wav = Vec::with_capacity(44 + data_size as usize);
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + data_size).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16_u32.to_le_bytes());
            wav.extend_from_slice(&1_u16.to_le_bytes());
            wav.extend_from_slice(&channels.to_le_bytes());
            wav.extend_from_slice(&44_100_u32.to_le_bytes());
            wav.extend_from_slice(&(44_100_u32 * u32::from(channels) * 2).to_le_bytes());
            wav.extend_from_slice(&(channels * 2).to_le_bytes());
            wav.extend_from_slice(&16_u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&data_size.to_le_bytes());
            for frame in 0..frames {
                let time = f64::from(frame) / 44_100.0;
                let left = (0.2 * (std::f64::consts::TAU * 1_000.0 * time).sin() * 32_767.0) as i16;
                let right =
                    (0.1 * (std::f64::consts::TAU * 2_000.0 * time).sin() * 32_767.0) as i16;
                wav.extend_from_slice(&left.to_le_bytes());
                if channels == 2 {
                    wav.extend_from_slice(&right.to_le_bytes());
                }
            }
            let wav_path = self.0.join("tone.wav");
            let aac_path = self.0.join("tone.m4a");
            std::fs::write(&wav_path, wav).unwrap();
            let result = std::process::Command::new("/usr/bin/afconvert")
                .args(["-f", "m4af", "-d", "aac ", "-q", "127", "-b", "256000"])
                .arg(wav_path)
                .arg(&aac_path)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "afconvert: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            aac_path
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for SongTestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(target_os = "macos")]
    fn test_rms_dbfs(samples: &[f32]) -> f64 {
        10.0 * (samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / samples.len() as f64)
            .log10()
    }

    #[cfg(target_os = "macos")]
    fn test_tone_frequency(samples: &[f32]) -> f64 {
        let interior = &samples[2_400..samples.len() - 2_400];
        let crossings = interior
            .windows(2)
            .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
            .count();
        crossings as f64 * f64::from(SONG_SAMPLE_RATE_HZ) / interior.len() as f64
    }

    #[test]
    fn live_music_nominal_level_uses_calibrated_drive() {
        let asset = PreparedAsset::live_music();
        let drive = fightbox_api::SceneCalibration::default()
            .derive_source_drive(
                fightbox_api::ReferenceLevel::SplAtOneMeter { db_spl: 125.0 },
                &asset.analysis,
            )
            .unwrap();
        assert!((20.0 * drive.linear_gain().log10() + 5.0).abs() < 0.001);
    }

    #[test]
    fn generated_asset_loads_for_headless_input() {
        let asset = load_asset("s0-approach-sine-1k").unwrap();
        assert_eq!(asset.samples.len(), 4_800);
        assert!(asset.samples.iter().any(|sample| *sample != 0.0));
    }

    #[test]
    fn wav_asset_loads_for_headless_input() {
        let asset = load_asset("toms-diner").unwrap();
        assert!(!asset.samples.is_empty());
        assert!(asset.samples.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn s7_generated_assets_parse_validate_and_load() {
        for asset_id in ["s7-siren", "s7-bell"] {
            let asset = load_asset(asset_id).unwrap();
            assert!(!asset.samples.is_empty(), "{asset_id}");
            assert!(
                asset.samples.iter().all(|sample| sample.is_finite()),
                "{asset_id}"
            );
        }
    }

    #[cfg(feature = "live-output")]
    #[test]
    fn live_wav_preserves_stereo_and_duplicates_mono() {
        let directory = std::env::temp_dir().join(format!(
            "fightbox-live-wav-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();

        let stereo_path = directory.join("stereo.wav");
        std::fs::write(&stereo_path, pcm16_wav(2, &[[8192, -16384], [32767, 0]])).unwrap();
        let (rate, stereo) = load_live_input_wav(&stereo_path).unwrap();
        assert_eq!(rate, 48_000);
        assert_eq!(stereo.len(), 2);
        assert_eq!(stereo[0], [0.25, -0.5]);
        assert!((stereo[1][0] - 32767.0 / 32768.0).abs() < f32::EPSILON);
        assert_eq!(stereo[1][1], 0.0);

        let mono_path = directory.join("mono.wav");
        std::fs::write(&mono_path, pcm16_wav(1, &[[8192, 0], [-16384, 0]])).unwrap();
        let (rate, mono) = load_live_input_wav(&mono_path).unwrap();
        assert_eq!(rate, 48_000);
        assert_eq!(mono, [[0.25, 0.25], [-0.5, -0.5]]);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(feature = "live-output")]
    fn pcm16_wav(channels: u16, frames: &[[i16; 2]]) -> Vec<u8> {
        let sample_count = frames.len() * usize::from(channels);
        let data_size = (sample_count * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_size as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_size).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&48_000_u32.to_le_bytes());
        wav.extend_from_slice(&(48_000_u32 * u32::from(channels) * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());
        for [left, right] in frames {
            wav.extend_from_slice(&left.to_le_bytes());
            if channels == 2 {
                wav.extend_from_slice(&right.to_le_bytes());
            }
        }
        wav
    }

    #[test]
    #[ignore = "requires the local Wave 17 audition preparation scripts"]
    fn prepared_wave17_audition_scene_assets_load_as_finite_mono() {
        let fixture: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/city/wave17-gamma-audition/fixture.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let asset_ids = fixture["sources"].as_array().unwrap().iter().map(|source| {
            source["asset_id"]
                .as_str()
                .expect("audition asset id is a string")
        });
        for asset_id in asset_ids {
            let asset = load_asset(asset_id).unwrap_or_else(|error| panic!("{asset_id}: {error}"));
            assert!(!asset.samples.is_empty(), "{asset_id}");
            assert!(
                asset.samples.iter().all(|sample| sample.is_finite()),
                "{asset_id}"
            );
            assert!(asset.analysis.program_rms_dbfs.is_finite(), "{asset_id}");
            assert!(asset.analysis.true_peak_dbtp.is_finite(), "{asset_id}");
        }
    }

    #[test]
    #[ignore = "requires local gitignored Squad WAVs prepared by tools/prepare-squad-assets.py"]
    fn prepared_squad_descriptors_round_trip_through_the_workbench_loader() {
        for asset_id in [
            "squad-abrams-idle",
            "squad-ural-idle",
            "squad-generator-diesel",
            "squad-fire-car",
            "squad-fire-building-large",
            "squad-fob-radio-static",
            "squad-camo-tent-flap",
            "squad-mi8-rotor-close",
            "squad-m2-blast",
            "squad-m2-burst-loop",
            "squad-dshk-burst-loop",
            "squad-a10-pass",
            "squad-a10-impacts",
        ] {
            let asset = load_asset(asset_id).unwrap_or_else(|error| panic!("{asset_id}: {error}"));
            assert!(!asset.samples.is_empty(), "{asset_id}");
            assert!(
                asset.samples.iter().all(|sample| sample.is_finite()),
                "{asset_id}"
            );
            assert!(asset.analysis.program_rms_dbfs.is_finite(), "{asset_id}");
            assert!(asset.analysis.true_peak_dbtp.is_finite(), "{asset_id}");
        }
    }
}
