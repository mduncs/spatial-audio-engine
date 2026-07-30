//! Bridges between `fightbox-evidence` metric types and the CLI's serde
//! metrics sidecars. Keeps the spectral/level/high-band computations in the
//! evidence layer and only mirrors the results into deterministic JSON structs.

use fightbox_evidence::{ChannelMetrics, WavSpec, channel_metrics};
use fightbox_steam_audio::{
    DirectOcclusionMode, DirectSnapshot, PathDirectionEstimate, PathSnapshot, ReflectionSnapshot,
};

use crate::bundle::{
    CalibrationPayload, ChannelMetricPayload, DirectSnapshotPayload, OcclusionModeKind,
    OcclusionModePayload, PathDirectionPayload, PathSnapshotPayload, PathingComparisonPayload,
    ReflectionSnapshotPayload, StemHashPayload,
};

/// Build a channel-health payload from an interleaved stereo buffer.
pub(crate) fn stereo_channel_metric(
    sample_rate_hz: u32,
    interleaved: &[f32],
) -> Result<ChannelMetricPayload, String> {
    let spec = WavSpec {
        sample_rate_hz,
        channels: 2,
    };
    let metrics = channel_metrics(spec, interleaved).map_err(|e| match e {
        fightbox_evidence::MetricError::BadSpec => "invalid WAV spec".to_string(),
        fightbox_evidence::MetricError::FrameChannelMismatch => {
            "stereo sample count is not a whole number of frames".to_string()
        }
        fightbox_evidence::MetricError::EmptyBins => "empty comparison bins".to_string(),
    })?;
    Ok(from_channel_metrics(&metrics))
}

pub(crate) fn from_channel_metrics(metrics: &ChannelMetrics) -> ChannelMetricPayload {
    ChannelMetricPayload {
        frame_count: metrics.frame_count,
        channels: metrics.channels,
        sample_rate_hz: metrics.sample_rate_hz,
        all_finite: metrics.all_finite,
        peak_per_channel: metrics.peak_per_channel.clone(),
        rms_per_channel: metrics.rms_per_channel.clone(),
        rms_dbfs_per_channel: metrics.rms_dbfs_per_channel.clone(),
        silent_channel_count: metrics.silent_channel_count,
        stereo_difference_rms: metrics.stereo_difference_rms,
    }
}

pub(crate) fn calibration_payload(
    reference_spl_db: f32,
    reference_pcm_rms_dbfs: f32,
    reference_distance_m: f32,
    program_rms_dbfs: f32,
    target_source_rms_dbfs: f32,
    drive_gain_db: f32,
    linear_gain: f32,
) -> CalibrationPayload {
    CalibrationPayload {
        reference_spl_db,
        reference_pcm_rms_dbfs,
        reference_distance_m,
        program_rms_dbfs,
        target_source_rms_dbfs,
        drive_gain_db,
        linear_gain,
    }
}

pub(crate) fn direct_snapshot_payload(snapshot: &DirectSnapshot) -> DirectSnapshotPayload {
    DirectSnapshotPayload {
        distance_attenuation: snapshot.distance_attenuation,
        air_absorption: snapshot.air_absorption,
        directivity: snapshot.directivity,
        occlusion: snapshot.occlusion,
        transmission: snapshot.transmission,
        requested_occlusion_mode: occlusion_mode_payload(snapshot.requested_occlusion_mode),
        delivered_occlusion_mode: occlusion_mode_payload(snapshot.delivered_occlusion_mode),
    }
}

/// Mirror a backend occlusion mode into the metrics sidecar payload.
pub(crate) fn occlusion_mode_payload(mode: DirectOcclusionMode) -> OcclusionModePayload {
    match mode {
        DirectOcclusionMode::Raycast => OcclusionModePayload {
            kind: OcclusionModeKind::Raycast,
            volumetric_radius_m: 0.0,
            volumetric_sample_count: 0,
        },
        DirectOcclusionMode::Volumetric {
            radius_m,
            sample_count,
        } => OcclusionModePayload {
            kind: OcclusionModeKind::Volumetric,
            volumetric_radius_m: radius_m,
            volumetric_sample_count: sample_count,
        },
    }
}

pub(crate) fn path_snapshot_payload(snapshot: &PathSnapshot) -> PathSnapshotPayload {
    PathSnapshotPayload {
        eq_coeffs: snapshot.eq_coeffs,
        sh_coeffs: snapshot.sh_coeffs.clone(),
        configured_order: snapshot.configured_order,
        direction: path_direction_payload(&snapshot.direction),
    }
}

pub(crate) fn path_direction_payload(
    direction: &Option<PathDirectionEstimate>,
) -> PathDirectionPayload {
    match direction {
        Some(estimate) => PathDirectionPayload {
            mean_arrival_direction_enu: [
                estimate.mean_arrival_direction_enu.x,
                estimate.mean_arrival_direction_enu.y,
                estimate.mean_arrival_direction_enu.z,
            ],
            azimuth_degrees_clockwise_from_north: estimate.azimuth_degrees_clockwise_from_north,
            first_order_magnitude: estimate.first_order_magnitude,
            zeroth_order_coefficient: estimate.zeroth_order_coefficient,
            is_some: true,
        },
        None => PathDirectionPayload {
            mean_arrival_direction_enu: [0.0; 3],
            azimuth_degrees_clockwise_from_north: f32::NAN,
            first_order_magnitude: 0.0,
            zeroth_order_coefficient: 0.0,
            is_some: false,
        },
    }
}

pub(crate) fn reflection_snapshot_payload(
    snapshot: &ReflectionSnapshot,
) -> ReflectionSnapshotPayload {
    ReflectionSnapshotPayload {
        num_channels: snapshot.num_channels,
        ir_size: snapshot.ir_size,
        reverb_times: snapshot.reverb_times,
        eq: snapshot.eq,
        delay_samples: snapshot.delay_samples,
    }
}

pub(crate) fn pathing_comparison_payload(
    on_hash: String,
    off_hash: String,
    comparison: &fightbox_evidence::SpectralComparison,
) -> PathingComparisonPayload {
    PathingComparisonPayload {
        on_sum_hash_sha256: on_hash,
        off_sum_hash_sha256: off_hash,
        bins_hz: comparison.bins_hz.clone(),
        on_rms_dbfs: comparison.on_rms_dbfs,
        off_rms_dbfs: comparison.off_rms_dbfs,
        level_difference_db: comparison.level_difference_db,
        energy: comparison.energy.as_str().to_string(),
        spectral_l1_difference: comparison.spectral_l1_difference,
        spectral_l2_difference: comparison.spectral_l2_difference,
        differs: comparison.differs,
    }
}

pub(crate) fn stem_hash_payload(
    kind: &str,
    file: &str,
    hash: String,
    frame_count: usize,
) -> StemHashPayload {
    StemHashPayload {
        kind: kind.to_string(),
        file: file.to_string(),
        content_sha256: hash,
        frame_count,
    }
}

/// High-band energy of a stereo buffer above `cutoff_hz`, computed as the RMS of
/// the mono mixdown after a naive one-pole high-pass approximation. This is a
/// bounded evidence metric, not a perceptual measurement; it only has to be
/// monotonic enough to confirm that enabled air absorption does not *increase*
/// high-band energy at the same 100 m pose.
pub(crate) fn high_band_rms(sample_rate_hz: u32, interleaved: &[f32], cutoff_hz: f32) -> f32 {
    let channels = 2_usize;
    if interleaved.len() < channels * 2 || sample_rate_hz == 0 {
        return 0.0;
    }
    let frame_count = interleaved.len() / channels;
    // Mono mixdown.
    let mut mono = Vec::with_capacity(frame_count);
    for frame in 0..frame_count {
        let left = interleaved[frame * channels];
        let right = interleaved[frame * channels + 1];
        mono.push((left + right) * 0.5);
    }
    // One-pole high-pass: y[n] = x[n] - alpha * y[n-1], alpha = RC/(RC+dt).
    let dt = 1.0_f32 / sample_rate_hz as f32;
    let rc = 1.0_f32 / (2.0 * core::f32::consts::PI * cutoff_hz);
    let alpha = rc / (rc + dt);
    let mut previous = 0.0_f32;
    let mut sum_sq = 0.0_f64;
    for &sample in &mono {
        previous = sample - alpha * previous;
        sum_sq += (previous as f64) * (previous as f64);
    }
    (sum_sq / mono.len() as f64).sqrt() as f32
}
