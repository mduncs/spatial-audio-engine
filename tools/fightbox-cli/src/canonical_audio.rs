//! Deterministic source ingestion into indexed one-second planar chunks.
//!
//! This is authoring and worker-side code. It never runs on the audio callback.

use std::collections::VecDeque;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use fightbox_api::ExtentDescriptor;
use fightbox_evidence::sha256_hex;
use fightbox_runtime::backend::{MAX_ACTIVE_SOURCES, SpatialProgramBlock};
use serde::{Deserialize, Serialize};

use crate::asset::{
    AssetLayout, MONO_EXPANSION_FAR_DELAY_FRAMES, MONO_EXPANSION_NEAR_DELAY_FRAMES,
    MONO_EXPANSION_SIDE_GAIN_DENOMINATOR, MONO_EXPANSION_SIDE_GAIN_NUMERATOR, MediaContainer,
    PresentationProvenance, SourceAssetDescriptor, SourceGeometry, SourcePresentation,
    decode_source_wav,
};
use crate::atomicio::{AtomicDir, validate_output_path, write_bytes_plain, write_json_atomic};
use crate::error::{CliError, Result};

const PACKAGE_SCHEMA: &str = "fightbox.canonical-audio-package.v1";
const SAMPLE_RATE_HZ: u32 = 48_000;
const CHUNK_FRAMES: usize = 48_000;
const ZSTD_LEVEL: i32 = 3;
const COMPRESSOR_REVISION: &str = "zstd-rust@0.13.3|level=3";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChunkEncoding {
    RawF32Le,
    ZstdF32Le,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalChunkRecord {
    index: u32,
    start_frame: u64,
    frame_count: u32,
    file: String,
    encoding: ChunkEncoding,
    uncompressed_bytes: u64,
    stored_bytes: u64,
    raw_sha256: String,
    stored_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalAudioPackageManifest {
    schema_version: String,
    source_descriptor_sha256: String,
    source_asset: SourceAssetDescriptor,
    sample_rate_hz: u32,
    channel_count: u16,
    frame_count: u64,
    chunk_frames: u32,
    compressor_revision: String,
    chunks: Vec<CanonicalChunkRecord>,
}

#[derive(Serialize)]
struct PackageSummary<'a> {
    schema_version: &'a str,
    asset_id: &'a str,
    artifact_id: &'a str,
    channel_count: u16,
    frame_count: u64,
    chunk_count: usize,
    stored_bytes: u64,
    raw_bytes: u64,
    verified: bool,
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("pack") => {
            let [descriptor, media, output] =
                parse_paths(&args[1..], ["--descriptor", "--media", "--output"])?;
            pack_wav(&descriptor, &media, &output)?;
            inspect(&output)
        }
        Some("inspect") if args.len() == 2 => inspect(Path::new(&args[1])),
        Some("inspect") => Err(CliError::new(
            "usage: fightbox asset inspect <canonical-package-directory>",
        )),
        Some(command) => Err(CliError::new(format!(
            "unknown asset subcommand {command}; expected pack or inspect"
        ))),
        None => Err(CliError::new(
            "asset requires a subcommand; expected pack or inspect",
        )),
    }
}

fn parse_paths<const N: usize>(args: &[String], names: [&str; N]) -> Result<[PathBuf; N]> {
    if args.len() != N * 2 {
        return Err(CliError::new(format!(
            "usage: fightbox asset pack {}",
            names
                .iter()
                .map(|name| format!("{name} <path>"))
                .collect::<Vec<_>>()
                .join(" ")
        )));
    }
    let mut values: [Option<PathBuf>; N] = std::array::from_fn(|_| None);
    for pair in args.chunks_exact(2) {
        let Some(index) = names.iter().position(|name| *name == pair[0]) else {
            return Err(CliError::new(format!(
                "unexpected asset pack option {}; expected {}",
                pair[0],
                names.join(", ")
            )));
        };
        if values[index].is_some() {
            return Err(CliError::new(format!(
                "asset pack option {} was supplied more than once",
                pair[0]
            )));
        }
        values[index] = Some(PathBuf::from(&pair[1]));
    }
    if values.iter().any(Option::is_none) {
        return Err(CliError::new(format!(
            "asset pack requires {}",
            names.join(", ")
        )));
    }
    Ok(values.map(|value| value.expect("all named asset paths validated")))
}

fn pack_wav(descriptor_path: &Path, media_path: &Path, output: &Path) -> Result<()> {
    let descriptor_text = std::fs::read_to_string(descriptor_path).map_err(|error| {
        CliError::new(format!(
            "cannot read source descriptor {}: {error}",
            descriptor_path.display()
        ))
    })?;
    let descriptor = SourceAssetDescriptor::parse(&descriptor_text)?;
    let media = std::fs::read(media_path).map_err(|error| {
        CliError::new(format!(
            "cannot read source media {}: {error}",
            media_path.display()
        ))
    })?;
    let media_hash = sha256_hex(&media);
    if media_hash != descriptor.original.content_sha256 {
        return Err(CliError::new(format!(
            "source media sha256 mismatch: descriptor {}, file {media_hash}",
            descriptor.original.content_sha256
        )));
    }
    if descriptor.original.format.container != MediaContainer::Wav {
        return Err(CliError::new(
            "this first importer tranche accepts PCM WAV; AIFF, CAF, FLAC, AAC/M4A, and MP3 remain scheduled adapters",
        ));
    }
    let decoded = decode_source_wav(&media, media_path)?;
    if decoded.sample_rate_hz != SAMPLE_RATE_HZ {
        return Err(CliError::new(format!(
            "this first importer tranche requires 48000 Hz input, got {} Hz; the pinned offline resampler remains scheduled",
            decoded.sample_rate_hz
        )));
    }
    if decoded.codec != descriptor.original.format.codec {
        return Err(CliError::new(
            "source media codec does not match descriptor original.format.codec",
        ));
    }
    if decoded.sample_rate_hz != descriptor.original.format.sample_rate_hz {
        return Err(CliError::new(
            "source media rate does not match descriptor original.format.sample_rate_hz",
        ));
    }
    let decoded_layout = match decoded.channels {
        1 => AssetLayout::Mono,
        2 => AssetLayout::StereoLR,
        _ => unreachable!("WAV decoder admits one or two channels"),
    };
    if decoded_layout != descriptor.layout || decoded_layout != descriptor.original.format.layout {
        return Err(CliError::new(
            "source media channel layout does not match the source descriptor",
        ));
    }
    let planar = deinterleave(decoded.channels, &decoded.samples)?;
    write_package(descriptor, &planar, output)
}

fn write_package(
    source_asset: SourceAssetDescriptor,
    planar: &[Vec<f32>],
    output: &Path,
) -> Result<()> {
    source_asset.validate()?;
    let channel_count = source_asset.layout.channels();
    if planar.len() != channel_count {
        return Err(CliError::new(format!(
            "canonical PCM has {} planes, descriptor requires {channel_count}",
            planar.len()
        )));
    }
    let frame_count = planar.first().map(Vec::len).unwrap_or(0);
    if frame_count == 0
        || planar.iter().any(|channel| channel.len() != frame_count)
        || planar.iter().flatten().any(|sample| !sample.is_finite())
    {
        return Err(CliError::new(
            "canonical planar PCM must be non-empty, finite, and equal-length",
        ));
    }
    if u64::try_from(frame_count).ok() != Some(source_asset.canonical.frame_count) {
        return Err(CliError::new(format!(
            "canonical frame count mismatch: descriptor {}, decoded {frame_count}",
            source_asset.canonical.frame_count
        )));
    }
    let canonical_hash = hash_complete_planar(planar);
    if canonical_hash != source_asset.canonical.canonical_pcm_sha256 {
        return Err(CliError::new(format!(
            "canonical PCM sha256 mismatch: descriptor {}, decoded {canonical_hash}",
            source_asset.canonical.canonical_pcm_sha256
        )));
    }

    let source_bytes = serde_json::to_vec(&source_asset)
        .map_err(|error| CliError::new(format!("cannot serialize source contract: {error}")))?;
    let final_path = validate_output_path(output)?;
    let atomic = AtomicDir::create(final_path)?;
    let stage = atomic.temp_path();
    std::fs::create_dir(stage.join("chunks")).map_err(|error| {
        CliError::new(format!(
            "cannot create canonical chunk directory {}: {error}",
            stage.join("chunks").display()
        ))
    })?;

    let mut chunks = Vec::with_capacity(frame_count.div_ceil(CHUNK_FRAMES));
    for (index, start) in (0..frame_count).step_by(CHUNK_FRAMES).enumerate() {
        let end = (start + CHUNK_FRAMES).min(frame_count);
        let raw = encode_chunk(planar, start, end);
        let compressed = zstd::stream::encode_all(Cursor::new(&raw), ZSTD_LEVEL)
            .map_err(|error| CliError::new(format!("zstd compression failed: {error}")))?;
        let (encoding, extension, stored) = if compressed.len() < raw.len() {
            (ChunkEncoding::ZstdF32Le, "f32le.zst", compressed)
        } else {
            (ChunkEncoding::RawF32Le, "f32le", raw.clone())
        };
        let file = format!("chunks/{index:08}.{extension}");
        write_bytes_plain(&stage.join(&file), &stored)?;
        chunks.push(CanonicalChunkRecord {
            index: u32::try_from(index)
                .map_err(|_| CliError::new("canonical chunk count exceeds u32"))?,
            start_frame: u64::try_from(start)
                .map_err(|_| CliError::new("canonical start frame exceeds u64"))?,
            frame_count: u32::try_from(end - start)
                .map_err(|_| CliError::new("canonical chunk frame count exceeds u32"))?,
            file,
            encoding,
            uncompressed_bytes: raw.len() as u64,
            stored_bytes: stored.len() as u64,
            raw_sha256: sha256_hex(&raw),
            stored_sha256: sha256_hex(&stored),
        });
    }
    let manifest = CanonicalAudioPackageManifest {
        schema_version: PACKAGE_SCHEMA.into(),
        source_descriptor_sha256: sha256_hex(&source_bytes),
        source_asset,
        sample_rate_hz: SAMPLE_RATE_HZ,
        channel_count: u16::try_from(channel_count)
            .map_err(|_| CliError::new("canonical channel count exceeds u16"))?,
        frame_count: frame_count as u64,
        chunk_frames: CHUNK_FRAMES as u32,
        compressor_revision: COMPRESSOR_REVISION.into(),
        chunks,
    };
    write_json_atomic(&stage.join("source-asset.json"), &manifest.source_asset)?;
    write_json_atomic(&stage.join("manifest.json"), &manifest)?;
    atomic.commit()
}

fn inspect(package: &Path) -> Result<()> {
    let reader = CanonicalAudioReader::open(package)?;
    reader.verify_all()?;
    let manifest = &reader.manifest;
    let summary = PackageSummary {
        schema_version: &manifest.schema_version,
        asset_id: &manifest.source_asset.asset_id,
        artifact_id: &manifest.source_asset.canonical.artifact_id,
        channel_count: manifest.channel_count,
        frame_count: manifest.frame_count,
        chunk_count: manifest.chunks.len(),
        stored_bytes: manifest.chunks.iter().map(|chunk| chunk.stored_bytes).sum(),
        raw_bytes: manifest
            .chunks
            .iter()
            .map(|chunk| chunk.uncompressed_bytes)
            .sum(),
        verified: true,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&summary)
            .map_err(|error| CliError::new(format!("cannot serialize package summary: {error}")))?
    );
    Ok(())
}

fn deinterleave(channels: u16, interleaved: &[f32]) -> Result<Vec<Vec<f32>>> {
    let channels = usize::from(channels);
    if channels == 0 || interleaved.is_empty() || interleaved.len() % channels != 0 {
        return Err(CliError::new(
            "decoded PCM is empty or not a whole number of channel frames",
        ));
    }
    let frames = interleaved.len() / channels;
    let mut planar = (0..channels)
        .map(|_| Vec::with_capacity(frames))
        .collect::<Vec<_>>();
    for frame in interleaved.chunks_exact(channels) {
        for (channel, sample) in frame.iter().enumerate() {
            planar[channel].push(*sample);
        }
    }
    Ok(planar)
}

fn encode_chunk(planar: &[Vec<f32>], start: usize, end: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity((end - start) * planar.len() * size_of::<f32>());
    for channel in planar {
        for sample in &channel[start..end] {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }
    bytes
}

fn hash_complete_planar(planar: &[Vec<f32>]) -> String {
    let mut bytes =
        Vec::with_capacity(planar.iter().map(Vec::len).sum::<usize>() * size_of::<f32>());
    for channel in planar {
        for sample in channel {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
    }
    sha256_hex(&bytes)
}

#[derive(Clone)]
struct CanonicalAudioReader {
    root: PathBuf,
    manifest: CanonicalAudioPackageManifest,
}

impl CanonicalAudioReader {
    fn open(root: &Path) -> Result<Self> {
        let bytes = std::fs::read(root.join("manifest.json")).map_err(|error| {
            CliError::new(format!(
                "cannot read canonical manifest {}: {error}",
                root.join("manifest.json").display()
            ))
        })?;
        let manifest: CanonicalAudioPackageManifest = serde_json::from_slice(&bytes)
            .map_err(|error| CliError::new(format!("invalid canonical manifest: {error}")))?;
        validate_manifest(&manifest)?;
        Ok(Self {
            root: root.to_owned(),
            manifest,
        })
    }

    fn read_chunk(&self, index: usize) -> Result<Vec<Vec<f32>>> {
        let record = self
            .manifest
            .chunks
            .get(index)
            .ok_or_else(|| CliError::new(format!("canonical chunk {index} is out of range")))?;
        let stored = std::fs::read(self.root.join(&record.file)).map_err(|error| {
            CliError::new(format!(
                "cannot read canonical chunk {}: {error}",
                self.root.join(&record.file).display()
            ))
        })?;
        if stored.len() as u64 != record.stored_bytes || sha256_hex(&stored) != record.stored_sha256
        {
            return Err(CliError::new(format!(
                "canonical chunk {} stored-byte identity mismatch",
                record.index
            )));
        }
        let raw = match record.encoding {
            ChunkEncoding::RawF32Le => stored,
            ChunkEncoding::ZstdF32Le => zstd::stream::decode_all(Cursor::new(stored))
                .map_err(|error| CliError::new(format!("zstd decompression failed: {error}")))?,
        };
        if raw.len() as u64 != record.uncompressed_bytes || sha256_hex(&raw) != record.raw_sha256 {
            return Err(CliError::new(format!(
                "canonical chunk {} raw identity mismatch",
                record.index
            )));
        }
        decode_chunk(
            &raw,
            usize::from(self.manifest.channel_count),
            record.frame_count as usize,
        )
    }

    fn verify_all(&self) -> Result<()> {
        let mut planar = (0..usize::from(self.manifest.channel_count))
            .map(|_| Vec::with_capacity(self.manifest.frame_count as usize))
            .collect::<Vec<_>>();
        for index in 0..self.manifest.chunks.len() {
            let chunk = self.read_chunk(index)?;
            for (target, source) in planar.iter_mut().zip(chunk) {
                target.extend(source);
            }
        }
        if planar
            .iter()
            .any(|channel| channel.len() as u64 != self.manifest.frame_count)
        {
            return Err(CliError::new(
                "canonical chunks do not reconstruct the declared frame count",
            ));
        }
        let actual = hash_complete_planar(&planar);
        if actual != self.manifest.source_asset.canonical.canonical_pcm_sha256 {
            return Err(CliError::new(format!(
                "canonical aggregate sha256 mismatch: manifest {}, reconstructed {actual}",
                self.manifest.source_asset.canonical.canonical_pcm_sha256
            )));
        }
        Ok(())
    }
}

fn validate_manifest(manifest: &CanonicalAudioPackageManifest) -> Result<()> {
    if manifest.schema_version != PACKAGE_SCHEMA
        || manifest.sample_rate_hz != SAMPLE_RATE_HZ
        || manifest.chunk_frames != CHUNK_FRAMES as u32
        || manifest.compressor_revision != COMPRESSOR_REVISION
    {
        return Err(CliError::new(
            "unsupported canonical audio package contract",
        ));
    }
    manifest.source_asset.validate()?;
    if usize::from(manifest.channel_count) != manifest.source_asset.layout.channels()
        || manifest.frame_count != manifest.source_asset.canonical.frame_count
    {
        return Err(CliError::new(
            "canonical manifest shape disagrees with source contract",
        ));
    }
    let descriptor_bytes = serde_json::to_vec(&manifest.source_asset)
        .map_err(|error| CliError::new(format!("cannot hash source contract: {error}")))?;
    if sha256_hex(&descriptor_bytes) != manifest.source_descriptor_sha256 {
        return Err(CliError::new(
            "canonical manifest source descriptor identity mismatch",
        ));
    }
    let expected_chunks = (manifest.frame_count as usize).div_ceil(CHUNK_FRAMES);
    if manifest.chunks.len() != expected_chunks {
        return Err(CliError::new(
            "canonical manifest chunk count does not cover its frame count",
        ));
    }
    for (index, chunk) in manifest.chunks.iter().enumerate() {
        let expected_start = index * CHUNK_FRAMES;
        let expected_frames = CHUNK_FRAMES.min(manifest.frame_count as usize - expected_start);
        let expected_file_prefix = format!("chunks/{index:08}.");
        if chunk.index as usize != index
            || chunk.start_frame as usize != expected_start
            || chunk.frame_count as usize != expected_frames
            || !chunk.file.starts_with(&expected_file_prefix)
            || chunk.file.contains("..")
            || Path::new(&chunk.file).is_absolute()
            || chunk.uncompressed_bytes
                != (expected_frames * usize::from(manifest.channel_count) * size_of::<f32>()) as u64
        {
            return Err(CliError::new(format!(
                "canonical chunk {index} violates index, range, path, or size invariants"
            )));
        }
    }
    Ok(())
}

fn decode_chunk(raw: &[u8], channels: usize, frames: usize) -> Result<Vec<Vec<f32>>> {
    let expected = channels
        .checked_mul(frames)
        .and_then(|samples| samples.checked_mul(size_of::<f32>()))
        .ok_or_else(|| CliError::new("canonical chunk byte count overflow"))?;
    if raw.len() != expected {
        return Err(CliError::new(
            "canonical chunk byte length does not match its shape",
        ));
    }
    let mut planar = (0..channels)
        .map(|_| Vec::with_capacity(frames))
        .collect::<Vec<_>>();
    for (channel, output) in planar.iter_mut().enumerate() {
        let start = channel * frames * size_of::<f32>();
        for bytes in raw[start..start + frames * size_of::<f32>()].chunks_exact(4) {
            let sample = f32::from_le_bytes(bytes.try_into().expect("four-byte f32"));
            if !sample.is_finite() {
                return Err(CliError::new("canonical chunk contains non-finite PCM"));
            }
            output.push(sample);
        }
    }
    Ok(planar)
}

/// Worker-side bounded cache. Each resident entry is exactly one second of
/// decoded planar PCM, so a capacity of two through four implements the V1
/// active-source residency target without callback decode or filesystem work.
struct CanonicalChunkCache {
    reader: CanonicalAudioReader,
    capacity_chunks: usize,
    resident: VecDeque<(usize, Vec<Vec<f32>>)>,
}

impl CanonicalChunkCache {
    fn new(reader: CanonicalAudioReader, capacity_chunks: usize) -> Result<Self> {
        if capacity_chunks == 0 {
            return Err(CliError::new(
                "canonical chunk cache capacity must be positive",
            ));
        }
        Ok(Self {
            reader,
            capacity_chunks,
            resident: VecDeque::with_capacity(capacity_chunks),
        })
    }

    fn read_window(&mut self, start_frame: u64, frame_count: usize) -> Result<Vec<Vec<f32>>> {
        let end_frame = start_frame
            .checked_add(frame_count as u64)
            .ok_or_else(|| CliError::new("canonical read window overflows u64"))?;
        if end_frame > self.reader.manifest.frame_count {
            return Err(CliError::new(
                "canonical read window extends beyond the source asset",
            ));
        }
        let channels = usize::from(self.reader.manifest.channel_count);
        let mut output = (0..channels)
            .map(|_| Vec::with_capacity(frame_count))
            .collect::<Vec<_>>();
        if frame_count == 0 {
            return Ok(output);
        }
        let first = start_frame as usize / CHUNK_FRAMES;
        let last = (end_frame as usize - 1) / CHUNK_FRAMES;
        for index in first..=last {
            self.promote(index)?;
            let record = &self.reader.manifest.chunks[index];
            let chunk_start = record.start_frame;
            let copy_start = start_frame.max(chunk_start) - chunk_start;
            let copy_end = end_frame.min(chunk_start + u64::from(record.frame_count)) - chunk_start;
            let chunk = self
                .resident
                .iter()
                .find(|(resident_index, _)| *resident_index == index)
                .map(|(_, chunk)| chunk)
                .expect("promoted canonical chunk is resident");
            for (target, source) in output.iter_mut().zip(chunk) {
                target.extend_from_slice(&source[copy_start as usize..copy_end as usize]);
            }
        }
        Ok(output)
    }

    fn promote(&mut self, index: usize) -> Result<()> {
        if let Some(position) = self
            .resident
            .iter()
            .position(|(resident_index, _)| *resident_index == index)
        {
            let entry = self
                .resident
                .remove(position)
                .expect("located cache entry exists");
            self.resident.push_back(entry);
            return Ok(());
        }
        let chunk = self.reader.read_chunk(index)?;
        if self.resident.len() == self.capacity_chunks {
            self.resident.pop_front();
        }
        self.resident.push_back((index, chunk));
        Ok(())
    }
}

/// Control/worker-thread adapter from an admitted canonical package to the
/// runtime's narrow planar program-block seam. Opening and reading perform
/// filesystem, hash, decode, cache, and allocation work, so neither method may
/// run on the audio callback. Returned windows own their samples; the producer
/// must retain and eventually drop them off-callback while the callback borrows
/// their planes allocation-free.
#[allow(dead_code)]
pub(crate) struct CanonicalProgramAdapter {
    cache: CanonicalChunkCache,
    source_index: usize,
    extent: ExtentDescriptor,
    presentation: SourcePresentation,
    asset_id: String,
    artifact_id: String,
}

#[allow(dead_code)]
impl CanonicalProgramAdapter {
    pub(crate) fn open(
        package: &Path,
        source_index: usize,
        extent: ExtentDescriptor,
        capacity_chunks: usize,
    ) -> Result<Self> {
        if source_index >= MAX_ACTIVE_SOURCES {
            return Err(CliError::new(format!(
                "canonical program source_index {source_index} exceeds runtime capacity {MAX_ACTIVE_SOURCES}"
            )));
        }
        extent.validate().map_err(|error| {
            CliError::new(format!("canonical program extent is invalid: {error:?}"))
        })?;
        if !(2..=4).contains(&capacity_chunks) {
            return Err(CliError::new(
                "canonical runtime cache capacity must be 2..=4 one-second chunks",
            ));
        }

        let reader = CanonicalAudioReader::open(package)?;
        let geometry = geometry_for_extent(extent);
        let presentation = reader
            .manifest
            .source_asset
            .admit_presentation(geometry)
            .map_err(|error| CliError::new(error.to_string()))?;
        let program_plane_count = usize::from(reader.manifest.channel_count);
        let stored_plane_count = match presentation {
            SourcePresentation::MonoExpandedStereoImage => 1,
            _ => presentation.program_plane_count(),
        };
        if program_plane_count != stored_plane_count {
            return Err(CliError::new(format!(
                "canonical program has {program_plane_count} stored planes but admitted presentation requires {stored_plane_count}",
            )));
        }
        let asset_id = reader.manifest.source_asset.asset_id.clone();
        let artifact_id = reader.manifest.source_asset.canonical.artifact_id.clone();
        Ok(Self {
            cache: CanonicalChunkCache::new(reader, capacity_chunks)?,
            source_index,
            extent,
            presentation,
            asset_id,
            artifact_id,
        })
    }

    #[must_use]
    pub(crate) fn source_index(&self) -> usize {
        self.source_index
    }

    #[must_use]
    pub(crate) fn extent(&self) -> ExtentDescriptor {
        self.extent
    }

    #[must_use]
    pub(crate) fn presentation(&self) -> SourcePresentation {
        self.presentation
    }

    #[must_use]
    pub(crate) fn presentation_provenance(&self) -> PresentationProvenance {
        self.presentation.provenance()
    }

    #[must_use]
    pub(crate) fn asset_id(&self) -> &str {
        &self.asset_id
    }

    #[must_use]
    pub(crate) fn artifact_id(&self) -> &str {
        &self.artifact_id
    }

    #[must_use]
    pub(crate) fn frame_count(&self) -> u64 {
        self.cache.reader.manifest.frame_count
    }

    pub(crate) fn read_window(
        &mut self,
        start_frame: u64,
        frame_count: usize,
    ) -> Result<CanonicalProgramWindow> {
        if frame_count == 0 {
            return Err(CliError::new(
                "canonical runtime program windows must contain at least one frame",
            ));
        }
        if self.presentation == SourcePresentation::MonoExpandedStereoImage {
            let planes = expand_mono_window(&mut self.cache, start_frame, frame_count)?;
            return Ok(CanonicalProgramWindow {
                source_index: self.source_index,
                start_frame,
                program_plane_count: 2,
                planes,
            });
        }

        let mut decoded = self.cache.read_window(start_frame, frame_count)?;
        let planes = match decoded.len() {
            1 => [decoded.pop().expect("one decoded plane"), Vec::new()],
            2 => {
                let right = decoded.pop().expect("right decoded plane");
                let left = decoded.pop().expect("left decoded plane");
                [left, right]
            }
            count => {
                return Err(CliError::new(format!(
                    "canonical runtime program decoded unsupported {count}-plane audio"
                )));
            }
        };
        Ok(CanonicalProgramWindow {
            source_index: self.source_index,
            start_frame,
            program_plane_count: self.presentation.program_plane_count(),
            planes,
        })
    }
}

/// Derive a seek-stable synthetic stereo window from canonical mono PCM.
/// Fixed absolute-frame taps mean reading across a package chunk boundary, or
/// seeking directly into the second chunk, produces identical output. The
/// canonical cache remains mono; only the returned worker-side window gains a
/// second plane.
fn expand_mono_window(
    cache: &mut CanonicalChunkCache,
    start_frame: u64,
    frame_count: usize,
) -> Result<[Vec<f32>; 2]> {
    let end_frame = start_frame
        .checked_add(frame_count as u64)
        .ok_or_else(|| CliError::new("mono expansion window overflows u64"))?;
    let history_start = start_frame.saturating_sub(MONO_EXPANSION_FAR_DELAY_FRAMES as u64);
    let contextual_frames = usize::try_from(end_frame - history_start)
        .map_err(|_| CliError::new("mono expansion context exceeds addressable memory"))?;
    let decoded = cache.read_window(history_start, contextual_frames)?;
    let mono = decoded
        .first()
        .ok_or_else(|| CliError::new("mono expansion source has no canonical plane"))?;
    if decoded.len() != 1 {
        return Err(CliError::new(
            "mono expansion requires exactly one canonical source plane",
        ));
    }

    let mut left = Vec::with_capacity(frame_count);
    let mut right = Vec::with_capacity(frame_count);
    let gain = f64::from(MONO_EXPANSION_SIDE_GAIN_NUMERATOR)
        / f64::from(MONO_EXPANSION_SIDE_GAIN_DENOMINATOR);
    for offset in 0..frame_count {
        let absolute_frame = start_frame + offset as u64;
        let center = mono[(absolute_frame - history_start) as usize];
        let near = delayed_sample(
            mono,
            history_start,
            absolute_frame,
            MONO_EXPANSION_NEAR_DELAY_FRAMES,
        );
        let far = delayed_sample(
            mono,
            history_start,
            absolute_frame,
            MONO_EXPANSION_FAR_DELAY_FRAMES,
        );
        let desired_side = (f64::from(near) - f64::from(far)) * gain;
        let (left_sample, right_sample) = exact_mono_pair(center, desired_side);
        left.push(left_sample);
        right.push(right_sample);
    }
    Ok([left, right])
}

fn delayed_sample(
    contextual_mono: &[f32],
    history_start: u64,
    absolute_frame: u64,
    delay_frames: usize,
) -> f32 {
    let delay_frames = delay_frames as u64;
    if absolute_frame < delay_frames {
        return 0.0;
    }
    let delayed_frame = absolute_frame - delay_frames;
    contextual_mono[(delayed_frame - history_start) as usize]
}

/// Round an L/R pair while preserving the original f32 center exactly under
/// both common mono-fold implementations. Nearly every sample accepts the
/// requested side on the first pass. At pathological sub-ULP center/side ratios
/// the side is halved until the center is representable, preferring faithful
/// mono over synthetic width.
fn exact_mono_pair(center: f32, mut side: f64) -> (f32, f32) {
    if center == 0.0 {
        // Preserve signed zero and avoid representing a large delayed side
        // around a center too small to survive a finite-precision mono fold.
        return (center, center);
    }
    let center_f64 = f64::from(center);
    for _ in 0..32 {
        let left = (center_f64 + side) as f32;
        let right = (center_f64 - side) as f32;
        if left.is_finite()
            && right.is_finite()
            && fold_mono_f32(left, right).to_bits() == center.to_bits()
            && fold_mono_f64(left, right).to_bits() == center.to_bits()
        {
            return (left, right);
        }
        side *= 0.5;
    }
    (center, center)
}

fn fold_mono_f32(left: f32, right: f32) -> f32 {
    left.mul_add(0.5, right * 0.5)
}

fn fold_mono_f64(left: f32, right: f32) -> f32 {
    ((f64::from(left) + f64::from(right)) * 0.5) as f32
}

const fn geometry_for_extent(extent: ExtentDescriptor) -> SourceGeometry {
    match extent {
        ExtentDescriptor::Point => SourceGeometry::Point,
        ExtentDescriptor::MultiPoint { .. } => SourceGeometry::MultiPoint,
        ExtentDescriptor::LineSegment { .. } => SourceGeometry::LineSegment,
        ExtentDescriptor::StereoImage { .. } => SourceGeometry::StereoImage,
    }
}

/// Owned decoded samples for one exact source-asset seek window. Plane zero is
/// mono or authored left; plane one is empty for mono and authored right for
/// stereo. This is exactly the ordering consumed by `SpatialProgramBlock` and
/// the neutral StereoImage backend path.
#[allow(dead_code)]
pub(crate) struct CanonicalProgramWindow {
    source_index: usize,
    start_frame: u64,
    program_plane_count: usize,
    planes: [Vec<f32>; 2],
}

#[allow(dead_code)]
impl CanonicalProgramWindow {
    #[must_use]
    pub(crate) fn start_frame(&self) -> u64 {
        self.start_frame
    }

    #[must_use]
    pub(crate) fn frame_count(&self) -> usize {
        self.planes[0].len()
    }

    #[must_use]
    pub(crate) fn program_planes(&self) -> [&[f32]; 2] {
        [&self.planes[0], &self.planes[1]]
    }

    #[must_use]
    pub(crate) fn as_spatial_program_block(&self) -> SpatialProgramBlock<'_> {
        SpatialProgramBlock {
            source_index: self.source_index,
            program_plane_count: self.program_plane_count,
            program_planes: self.program_planes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use fightbox_evidence::{WavSpec, write_wav};

    use super::*;
    use crate::asset::{
        AudioCodec, MediaContainer, PCA_CENTER_MONO_DERIVATIVE_RECIPE_ID, PresentationDerivation,
        mono_expansion_recipe_sha256,
    };

    static SERIAL: AtomicU64 = AtomicU64::new(0);

    struct PackedFixture {
        root: PathBuf,
        media: Option<PathBuf>,
        descriptor_path: Option<PathBuf>,
        planar: Vec<Vec<f32>>,
        descriptor: SourceAssetDescriptor,
    }

    impl Drop for PackedFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
            if let Some(media) = &self.media {
                let _ = std::fs::remove_file(media);
            }
            if let Some(descriptor_path) = &self.descriptor_path {
                let _ = std::fs::remove_file(descriptor_path);
            }
        }
    }

    fn unique_root(label: &str) -> PathBuf {
        let id = SERIAL.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("fightbox-{label}-{}-{id}", std::process::id()))
    }

    fn packed_authored_stereo_fixture() -> PackedFixture {
        let frames = CHUNK_FRAMES + 17;
        let mut interleaved = Vec::with_capacity(frames * 2);
        let mut planar = vec![Vec::with_capacity(frames), Vec::with_capacity(frames)];
        for frame in 0..frames {
            let phase = std::f32::consts::TAU * frame as f32 / SAMPLE_RATE_HZ as f32;
            let left = 0.25 * (440.0 * phase).sin();
            let right = 0.125 * (880.0 * phase).sin();
            interleaved.extend_from_slice(&[left, right]);
            planar[0].push(left);
            planar[1].push(right);
        }
        let wav = write_wav(
            WavSpec {
                sample_rate_hz: SAMPLE_RATE_HZ,
                channels: 2,
            },
            &interleaved,
        )
        .unwrap();
        let mut descriptor = SourceAssetDescriptor::parse(include_str!(
            "../../../fixtures/assets/source-contract-authored-stereo.json"
        ))
        .unwrap();
        descriptor.original.content_sha256 = sha256_hex(&wav);
        descriptor.original.format.container = MediaContainer::Wav;
        descriptor.original.format.codec = AudioCodec::PcmFloat;
        descriptor.original.format.sample_rate_hz = SAMPLE_RATE_HZ;
        descriptor.original.format.lossy = false;
        let canonical_hash = hash_complete_planar(&planar);
        descriptor.canonical.canonical_pcm_sha256 = canonical_hash.clone();
        descriptor.canonical.artifact_id =
            format!("fightbox.canonical-audio.v1:sha256:{canonical_hash}");
        descriptor.canonical.frame_count = frames as u64;
        descriptor.validate().unwrap();

        let root = unique_root("canonical-authored-stereo");
        let media = root.with_extension("wav");
        let descriptor_path = root.with_extension("json");
        std::fs::write(&media, &wav).unwrap();
        std::fs::write(
            &descriptor_path,
            serde_json::to_vec_pretty(&descriptor).unwrap(),
        )
        .unwrap();
        pack_wav(&descriptor_path, &media, &root).unwrap();
        PackedFixture {
            root,
            media: Some(media),
            descriptor_path: Some(descriptor_path),
            planar,
            descriptor,
        }
    }

    fn packed_native_mono_fixture() -> PackedFixture {
        let frames = 257;
        let planar = vec![
            (0..frames)
                .map(|frame| (frame as f32 * 0.031_25).sin() * 0.2)
                .collect::<Vec<_>>(),
        ];
        let mut descriptor = SourceAssetDescriptor::parse(include_str!(
            "../../../fixtures/assets/s0-approach-sine-1k.json"
        ))
        .unwrap();
        let canonical_hash = hash_complete_planar(&planar);
        descriptor.canonical.canonical_pcm_sha256 = canonical_hash.clone();
        descriptor.canonical.artifact_id =
            format!("fightbox.canonical-audio.v1:sha256:{canonical_hash}");
        descriptor.canonical.frame_count = frames as u64;
        descriptor.validate().unwrap();
        let root = unique_root("canonical-native-mono");
        write_package(descriptor.clone(), &planar, &root).unwrap();
        PackedFixture {
            root,
            media: None,
            descriptor_path: None,
            planar,
            descriptor,
        }
    }

    fn packed_mono_expanded_fixture() -> PackedFixture {
        let frames = CHUNK_FRAMES + MONO_EXPANSION_FAR_DELAY_FRAMES + 257;
        let mut state = 0x53a9_17cdu32;
        let planar = vec![
            (0..frames)
                .map(|frame| {
                    // Stable broadband material plus a musical center gives the
                    // delayed-difference side enough content for an audible
                    // width smoke without depending on an external asset.
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = ((state >> 8) as f32 / 16_777_215.0) * 2.0 - 1.0;
                    let phase = std::f32::consts::TAU * frame as f32 / SAMPLE_RATE_HZ as f32;
                    0.16 * (311.0 * phase).sin() + 0.06 * noise
                })
                .collect::<Vec<_>>(),
        ];
        let mut descriptor = SourceAssetDescriptor::parse(include_str!(
            "../../../fixtures/assets/s0-approach-sine-1k.json"
        ))
        .unwrap();
        let canonical_hash = hash_complete_planar(&planar);
        descriptor.canonical.canonical_pcm_sha256 = canonical_hash.clone();
        descriptor.canonical.artifact_id =
            format!("fightbox.canonical-audio.v1:sha256:{canonical_hash}");
        descriptor.canonical.frame_count = frames as u64;
        descriptor.presentation_provenance = PresentationProvenance::MonoExpanded;
        descriptor.compatible_geometries = vec![SourceGeometry::StereoImage];
        descriptor.derivation = Some(PresentationDerivation {
            source_artifact_id: descriptor.canonical.artifact_id.clone(),
            recipe_sha256: mono_expansion_recipe_sha256(),
        });
        descriptor.validate().unwrap();
        let root = unique_root("canonical-mono-expanded");
        write_package(descriptor.clone(), &planar, &root).unwrap();
        PackedFixture {
            root,
            media: None,
            descriptor_path: None,
            planar,
            descriptor,
        }
    }

    #[test]
    fn authored_stereo_adapter_seeks_bit_exactly_across_chunk_boundaries() {
        let fixture = packed_authored_stereo_fixture();
        let reader = CanonicalAudioReader::open(&fixture.root).unwrap();
        assert_eq!(reader.manifest.chunks.len(), 2);
        reader.verify_all().unwrap();

        let extent = ExtentDescriptor::StereoImage { width_m: 2.0 };
        let mut adapter = CanonicalProgramAdapter::open(&fixture.root, 2, extent, 2).unwrap();
        assert_eq!(adapter.source_index(), 2);
        assert_eq!(adapter.extent(), extent);
        assert_eq!(
            adapter.presentation(),
            SourcePresentation::AuthoredStereoImage
        );
        assert_eq!(
            adapter.presentation_provenance(),
            PresentationProvenance::AuthoredStereo
        );
        assert_eq!(adapter.asset_id(), fixture.descriptor.asset_id);
        assert_eq!(
            adapter.artifact_id(),
            fixture.descriptor.canonical.artifact_id
        );
        assert_eq!(adapter.frame_count(), fixture.planar[0].len() as u64);

        let start = (CHUNK_FRAMES - 3) as u64;
        let first = adapter.read_window(start, 8).unwrap();
        assert_eq!(first.start_frame(), start);
        assert_eq!(first.frame_count(), 8);
        assert_eq!(
            first.program_planes()[0],
            &fixture.planar[0][CHUNK_FRAMES - 3..CHUNK_FRAMES + 5]
        );
        assert_eq!(
            first.program_planes()[1],
            &fixture.planar[1][CHUNK_FRAMES - 3..CHUNK_FRAMES + 5]
        );
        let first_bits = first.program_planes().map(|plane| {
            plane
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        });
        drop(first);

        let earlier = adapter.read_window(29, 11).unwrap();
        assert_eq!(earlier.program_planes()[0], &fixture.planar[0][29..40]);
        drop(earlier);
        let repeated = adapter.read_window(start, 8).unwrap();
        let repeated_bits = repeated.program_planes().map(|plane| {
            plane
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        });
        assert_eq!(repeated_bits, first_bits);
        let block = repeated.as_spatial_program_block();
        assert_eq!(block.source_index, 2);
        assert_eq!(block.program_plane_count, 2);
        assert_eq!(block.program_planes[0], repeated.program_planes()[0]);
        assert_eq!(block.program_planes[1], repeated.program_planes()[1]);

        let point_error =
            CanonicalProgramAdapter::open(&fixture.root, 2, ExtentDescriptor::Point, 2)
                .err()
                .expect("authored stereo Point must reject");
        assert!(point_error.message().contains("PCA-center mono derivative"));
        assert!(
            point_error
                .message()
                .contains(PCA_CENTER_MONO_DERIVATIVE_RECIPE_ID)
        );
        assert!(
            point_error
                .message()
                .contains(&fixture.descriptor.canonical.artifact_id)
        );
        let line_error = CanonicalProgramAdapter::open(
            &fixture.root,
            2,
            ExtentDescriptor::LineSegment { length_m: 2.0 },
            2,
        )
        .err()
        .expect("authored stereo LineSegment must reject");
        assert!(line_error.message().contains("deferred in V1"));
    }

    #[test]
    fn native_mono_point_adapter_keeps_one_plane_and_empty_plane_one() {
        let fixture = packed_native_mono_fixture();
        let mut adapter =
            CanonicalProgramAdapter::open(&fixture.root, 0, ExtentDescriptor::Point, 2).unwrap();
        assert_eq!(
            adapter.presentation(),
            SourcePresentation::NativeMono {
                geometry: SourceGeometry::Point
            }
        );
        assert_eq!(
            adapter.presentation_provenance(),
            PresentationProvenance::NativeMono
        );
        let window = adapter.read_window(17, 64).unwrap();
        let block = window.as_spatial_program_block();
        assert_eq!(block.program_plane_count, 1);
        assert_eq!(block.program_planes[0], &fixture.planar[0][17..81]);
        assert!(block.program_planes[1].is_empty());

        // A native-mono package cannot silently opt into StereoImage. The
        // synthetic presentation remains an explicit derived descriptor.
        assert!(
            CanonicalProgramAdapter::open(
                &fixture.root,
                0,
                ExtentDescriptor::StereoImage { width_m: 2.0 },
                2,
            )
            .is_err()
        );
    }

    #[test]
    fn mono_expansion_is_seek_stable_across_chunks_and_folds_bit_exactly() {
        let fixture = packed_mono_expanded_fixture();
        let extent = ExtentDescriptor::StereoImage { width_m: 2.0 };
        let mut adapter = CanonicalProgramAdapter::open(&fixture.root, 3, extent, 2).unwrap();
        assert_eq!(
            adapter.presentation(),
            SourcePresentation::MonoExpandedStereoImage
        );
        assert_eq!(
            adapter.presentation_provenance(),
            PresentationProvenance::MonoExpanded
        );

        let start = (CHUNK_FRAMES - 31) as u64;
        let whole = adapter.read_window(start, 96).unwrap();
        let whole_bits = whole.program_planes().map(|plane| {
            plane
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        });
        assert_eq!(whole.as_spatial_program_block().program_plane_count, 2);
        for (offset, (&left, &right)) in whole.program_planes()[0]
            .iter()
            .zip(whole.program_planes()[1])
            .enumerate()
        {
            let center = fixture.planar[0][start as usize + offset];
            assert_eq!(fold_mono_f32(left, right).to_bits(), center.to_bits());
            assert_eq!(fold_mono_f64(left, right).to_bits(), center.to_bits());
        }
        drop(whole);

        // Separate reads on opposite sides of the one-second package boundary
        // concatenate to precisely the same derived bytes as the spanning read.
        let before = adapter.read_window(start, 31).unwrap();
        let before_bits = before.program_planes().map(|plane| {
            plane
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        });
        drop(before);
        let after = adapter.read_window(CHUNK_FRAMES as u64, 65).unwrap();
        let after_bits = after.program_planes().map(|plane| {
            plane
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>()
        });
        for plane in 0..2 {
            let mut joined = before_bits[plane].clone();
            joined.extend_from_slice(&after_bits[plane]);
            assert_eq!(joined, whole_bits[plane]);
        }
    }

    #[test]
    fn mono_expansion_has_bounded_audible_side_without_claiming_authored_stereo() {
        let fixture = packed_mono_expanded_fixture();
        let start = MONO_EXPANSION_FAR_DELAY_FRAMES as u64;
        let frames = 24_000;
        let mut adapter = CanonicalProgramAdapter::open(
            &fixture.root,
            1,
            ExtentDescriptor::StereoImage { width_m: 3.0 },
            2,
        )
        .unwrap();
        let window = adapter.read_window(start, frames).unwrap();
        let [left, right] = window.program_planes();
        let mut center_energy = 0.0;
        let mut side_energy = 0.0;
        let mut left_energy = 0.0;
        let mut right_energy = 0.0;
        let mut cross = 0.0;
        for (&left, &right) in left.iter().zip(right) {
            let center = f64::from(fold_mono_f64(left, right));
            let side = (f64::from(left) - f64::from(right)) * 0.5;
            center_energy += center * center;
            side_energy += side * side;
            left_energy += f64::from(left).powi(2);
            right_energy += f64::from(right).powi(2);
            cross += f64::from(left) * f64::from(right);
        }
        let side_ratio = side_energy / center_energy;
        let correlation = cross / (left_energy * right_energy).sqrt();
        let side_rms_db = 10.0 * side_ratio.log10();
        println!(
            "mono-expanded pre-HRTF monitor: side {side_rms_db:.2} dB below center, L/R correlation {correlation:.3}, exact mono fold"
        );
        // ||x[n-a]-x[n-b]|| <= 2||x|| and gain=3/16 gives
        // a global side-energy ceiling of 9/64 (plus finite window edges).
        assert!(side_ratio > 0.002, "derived width must be audible/nonzero");
        assert!(side_ratio < 0.16, "side energy must remain bounded");
        assert!(correlation < 0.995, "two-plane image must not be dual mono");
        assert_eq!(fixture.descriptor.layout, AssetLayout::Mono);
        assert_eq!(
            fixture.descriptor.presentation_provenance,
            PresentationProvenance::MonoExpanded
        );
        assert_eq!(
            fixture
                .descriptor
                .derivation
                .as_ref()
                .unwrap()
                .recipe_sha256,
            mono_expansion_recipe_sha256()
        );
    }

    #[test]
    fn authored_stereo_image_planes_pass_an_audible_channel_pan_smoke() {
        let fixture = packed_authored_stereo_fixture();
        let mut adapter = CanonicalProgramAdapter::open(
            &fixture.root,
            1,
            ExtentDescriptor::StereoImage { width_m: 2.0 },
            2,
        )
        .unwrap();
        // 4,800 frames contain exact whole cycles of the authored 440 Hz left
        // and 880 Hz right tones. The neutral backend maps these planes to
        // WidthNegative and WidthPositive respectively; this pre-HRTF monitor
        // smoke proves both audible channels and their authored level contrast.
        let window = adapter.read_window(0, 4_800).unwrap();
        let [left, right] = window.program_planes();
        let left_energy = left
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / left.len() as f64;
        let right_energy = right
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / right.len() as f64;
        let correlation = left
            .iter()
            .zip(right)
            .map(|(left, right)| f64::from(*left) * f64::from(*right))
            .sum::<f64>()
            / ((left_energy * right_energy).sqrt() * left.len() as f64);
        let pan = (right_energy - left_energy) / (right_energy + left_energy);
        let left_rms_dbfs = 10.0 * left_energy.log10();
        let right_rms_dbfs = 10.0 * right_energy.log10();
        println!(
            "authored-stereo pre-HRTF monitor: left {left_rms_dbfs:.2} dBFS RMS, right {right_rms_dbfs:.2} dBFS RMS, energy pan {pan:.3}, correlation {correlation:.6}"
        );
        assert!(left_rms_dbfs > -16.0);
        assert!(right_rms_dbfs > -22.5);
        assert!((-0.61..=-0.59).contains(&pan));
        assert!(correlation.abs() < 1.0e-5);
    }
}
