use std::io::{BufWriter, Write};
use std::path::Path;

use fightbox_api::{EnuVector3, Pose};
use fightbox_runtime::backend::{
    MAX_SPATIAL_ENVIRONMENT_PLANES, MAX_SPATIAL_PRESENTATION_FEEDS, SpatialEnvironmentalBasis,
    SpatialFeedPlacement, SpatialOutputMetadata, SpatialOutputValidity,
};

pub const CHANNELS: usize = 9;
pub const SAMPLE_RATE_HZ: u32 = 48_000;
const SQRT_3: f32 = 1.732_050_8;
const SQRT_4_PI: f32 = 3.544_907_8;
pub const MAX_SAMPLE: f32 = 0.891_250_9;

/// ACN/SN3D, with +X forward, +Y left, and +Z up.
pub fn encode_sn3d(direction: [f32; 3]) -> [f32; CHANNELS] {
    let [x, y, z] = normalized(direction).unwrap_or([1.0, 0.0, 0.0]);
    [
        1.0,
        y,
        z,
        x,
        SQRT_3 * x * y,
        SQRT_3 * y * z,
        0.5 * (3.0 * z * z - 1.0),
        SQRT_3 * x * z,
        0.5 * SQRT_3 * (x * x - y * y),
    ]
}

struct ListenerTransform {
    axes: [[f32; 3]; 3],
    environment: [[f32; CHANNELS]; CHANNELS],
}

impl ListenerTransform {
    fn new(listener: Pose, basis: SpatialEnvironmentalBasis) -> Result<Self, &'static str> {
        if !listener.position.is_finite() {
            return Err("AmbiX export received a non-finite listener position");
        }
        let front = normalized(enu(listener.forward))
            .ok_or("AmbiX export received an invalid listener forward vector")?;
        let left = normalized(cross(enu(listener.up), front))
            .ok_or("AmbiX export received an invalid listener up vector")?;
        let up = cross(front, left);
        let axes = [front, left, up];
        let rotation = match basis {
            SpatialEnvironmentalBasis::RightHandedEnu => axes,
            // Steam SH evaluates its Cartesian polynomials at
            // (-Steam.z, -Steam.x, Steam.y): world north, west, up.
            SpatialEnvironmentalBasis::RightHandedXRightYUpZBack => {
                axes.map(|axis| [axis[1], -axis[0], axis[2]])
            }
        };
        let mut environment = [[0.0; CHANNELS]; CHANNELS];
        for channel in 0..CHANNELS {
            let order = if channel == 0 {
                0
            } else if channel < 4 {
                1
            } else {
                2
            };
            let mut unit = [0.0; CHANNELS];
            let normalization = match basis {
                SpatialEnvironmentalBasis::RightHandedEnu => 1.0,
                // The pinned native Steam bank preserves orthonormal SH
                // coefficients (Y00 = 1/sqrt(4*pi)), unlike the ENU bank.
                SpatialEnvironmentalBasis::RightHandedXRightYUpZBack => SQRT_4_PI,
            };
            unit[channel] = normalization / ((2 * order + 1) as f32).sqrt();
            // Google SH includes the Condon–Shortley phase for odd |m|;
            // AmbiX and the canonical ENU N3D bank omit it.
            if basis == SpatialEnvironmentalBasis::RightHandedXRightYUpZBack
                && matches!(channel, 1 | 3 | 5 | 7)
            {
                unit[channel] = -unit[channel];
            }
            let rotated = rotate_sn3d(unit, rotation);
            for output in 0..CHANNELS {
                environment[output][channel] = rotated[output];
            }
        }
        Ok(Self { axes, environment })
    }

    fn direction(&self, world_enu: [f32; 3]) -> [f32; 3] {
        self.axes.map(|axis| dot(axis, world_enu))
    }
}

/// Combine the neutral route's calibrated source feeds and world field.
/// Banks are plane-major; `output` is caller-owned interleaved ACN/SN3D.
pub fn encode_block(
    presentation_bank: &[f32],
    environmental_bank: &[f32],
    metadata: &SpatialOutputMetadata,
    listener: Pose,
    frames: usize,
    output: &mut [f32],
) -> Result<(), &'static str> {
    if presentation_bank.len() != MAX_SPATIAL_PRESENTATION_FEEDS * frames
        || environmental_bank.len() != MAX_SPATIAL_ENVIRONMENT_PLANES * frames
        || output.len() != CHANNELS * frames
        || metadata.block_size_frames as usize != frames
        || metadata.sample_rate_hz != SAMPLE_RATE_HZ
    {
        return Err("AmbiX export received an invalid 48 kHz spatial block");
    }
    output.fill(0.0);
    if metadata.validity == SpatialOutputValidity::SilentDiscontinuity {
        return Ok(());
    }
    if metadata.validity != SpatialOutputValidity::Valid
        || !metadata.world_space_unrotated
        || metadata.monitor_gain_applied
        || metadata.final_hrtf_applied
        || metadata.output_limiter_applied
        || metadata.active_environmental_plane_count
            != metadata.active_environmental_order.channel_count()
        || metadata.active_environmental_plane_count > CHANNELS
    {
        return Err("AmbiX export requires an unrotated neutral spatial block at monitor 0 dB");
    }
    let transform = ListenerTransform::new(listener, metadata.environmental_basis)?;
    for (plane, feed) in metadata.presentation_feeds.iter().enumerate() {
        if !feed.valid {
            continue;
        }
        let direction = match feed.placement {
            SpatialFeedPlacement::Direction => enu(feed.direction_enu),
            SpatialFeedPlacement::Pose => [
                feed.pose_enu.position.east_m - listener.position.east_m,
                feed.pose_enu.position.north_m - listener.position.north_m,
                feed.pose_enu.position.up_m - listener.position.up_m,
            ],
        };
        if !direction.iter().all(|component| component.is_finite()) {
            return Err("AmbiX export received a non-finite source direction");
        }
        let direction = normalized(direction).unwrap_or([0.0, 1.0, 0.0]);
        let coefficients = encode_sn3d(transform.direction(direction));
        let input = &presentation_bank[plane * frames..(plane + 1) * frames];
        for (sample, frame) in input.iter().zip(output.chunks_exact_mut(CHANNELS)) {
            for channel in 0..CHANNELS {
                frame[channel] += sample * coefficients[channel];
            }
        }
    }
    for input_channel in 0..metadata.active_environmental_plane_count {
        let input = &environmental_bank[input_channel * frames..(input_channel + 1) * frames];
        for (sample, frame) in input.iter().zip(output.chunks_exact_mut(CHANNELS)) {
            for channel in 0..CHANNELS {
                frame[channel] += sample * transform.environment[channel][input_channel];
            }
        }
    }
    Ok(())
}

fn rotate_sn3d(coefficients: [f32; CHANNELS], rotation: [[f32; 3]; 3]) -> [f32; CHANNELS] {
    let vector = [coefficients[3], coefficients[1], coefficients[2]];
    let [x, y, z] = rotation.map(|axis| dot(axis, vector));
    // The five order-two channels are a symmetric traceless tensor. Its
    // rotation preserves order, including arbitrary listener pitch and roll.
    let q = [
        [
            -coefficients[6] / 3.0 + coefficients[8] / SQRT_3,
            coefficients[4] / SQRT_3,
            coefficients[7] / SQRT_3,
        ],
        [
            coefficients[4] / SQRT_3,
            -coefficients[6] / 3.0 - coefficients[8] / SQRT_3,
            coefficients[5] / SQRT_3,
        ],
        [
            coefficients[7] / SQRT_3,
            coefficients[5] / SQRT_3,
            2.0 * coefficients[6] / 3.0,
        ],
    ];
    let mut rotated = [[0.0; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            for i in 0..3 {
                for j in 0..3 {
                    rotated[row][column] += rotation[row][i] * q[i][j] * rotation[column][j];
                }
            }
        }
    }
    [
        coefficients[0],
        y,
        z,
        x,
        SQRT_3 * rotated[0][1],
        SQRT_3 * rotated[1][2],
        1.5 * rotated[2][2],
        SQRT_3 * rotated[0][2],
        0.5 * SQRT_3 * (rotated[0][0] - rotated[1][1]),
    ]
}

fn enu(vector: EnuVector3) -> [f32; 3] {
    [vector.east_m, vector.north_m, vector.up_m]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalized(vector: [f32; 3]) -> Option<[f32; 3]> {
    let length_squared = dot(vector, vector);
    if !length_squared.is_finite() || length_squared <= 1.0e-12 {
        return None;
    }
    let inverse_length = length_squared.sqrt().recip();
    Some(vector.map(|component| component * inverse_length))
}

pub fn write_ambix_wav(path: &Path, samples: &[f32]) -> Result<(), String> {
    if samples.len() % CHANNELS != 0 {
        return Err("AmbiX WAV requires complete nine-channel frames".into());
    }
    let mut peak = 0.0_f32;
    for &sample in samples {
        if !sample.is_finite() {
            return Err("AmbiX export failed: a sample is non-finite; no file was written".into());
        }
        peak = peak.max(sample.abs());
    }
    if peak > MAX_SAMPLE {
        return Err(format!(
            "AmbiX export failed: peak {:.2} dBFS exceeds the -1 dBFS ceiling; no file was written",
            20.0 * peak.log10()
        ));
    }
    let data_bytes = samples
        .len()
        .checked_mul(4)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .filter(|bytes| *bytes <= u32::MAX - 72)
        .ok_or("AmbiX WAV exceeds the RIFF size limit")?;
    let frames = u32::try_from(samples.len() / CHANNELS)
        .map_err(|_| "AmbiX WAV frame count exceeds the RIFF size limit")?;
    let file = std::fs::File::create(path)
        .map_err(|error| format!("cannot create AmbiX WAV {}: {error}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(&wav_header(data_bytes, frames))
        .map_err(|error| format!("cannot write AmbiX WAV header: {error}"))?;
    for sample in samples {
        writer
            .write_all(&sample.to_le_bytes())
            .map_err(|error| format!("cannot write AmbiX WAV samples: {error}"))?;
    }
    writer
        .flush()
        .map_err(|error| format!("cannot finish AmbiX WAV: {error}"))
}

fn wav_header(data_bytes: u32, frames: u32) -> [u8; 80] {
    let mut header = [0; 80];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(72 + data_bytes).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&40_u32.to_le_bytes());
    header[20..22].copy_from_slice(&0xfffe_u16.to_le_bytes());
    header[22..24].copy_from_slice(&(CHANNELS as u16).to_le_bytes());
    header[24..28].copy_from_slice(&SAMPLE_RATE_HZ.to_le_bytes());
    header[28..32].copy_from_slice(&(SAMPLE_RATE_HZ * CHANNELS as u32 * 4).to_le_bytes());
    header[32..34].copy_from_slice(&(CHANNELS as u16 * 4).to_le_bytes());
    header[34..36].copy_from_slice(&32_u16.to_le_bytes());
    header[36..38].copy_from_slice(&22_u16.to_le_bytes());
    header[38..40].copy_from_slice(&32_u16.to_le_bytes());
    // No speaker mask describes ACN channels. SubFormat is IEEE_FLOAT.
    header[44..60].copy_from_slice(&[
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ]);
    header[60..64].copy_from_slice(b"fact");
    header[64..68].copy_from_slice(&4_u32.to_le_bytes());
    header[68..72].copy_from_slice(&frames.to_le_bytes());
    header[72..76].copy_from_slice(b"data");
    header[76..80].copy_from_slice(&data_bytes.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: [f32; CHANNELS], expected: [f32; CHANNELS]) {
        for (channel, (actual, expected)) in actual.into_iter().zip(expected).enumerate() {
            assert!(
                (actual - expected).abs() < 2.0e-6,
                "ACN {channel}: {actual} != {expected}"
            );
        }
    }

    #[test]
    fn sn3d_cardinal_reference_values() {
        let s = 0.5 * SQRT_3;
        for (direction, expected) in [
            (
                [1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, 1.0, 0.0, 0.0, -0.5, 0.0, s],
            ),
            (
                [0.0, 1.0, 0.0],
                [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, -0.5, 0.0, -s],
            ),
            (
                [0.0, 0.0, 1.0],
                [1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            ),
            (
                [-1.0, 0.0, 0.0],
                [1.0, 0.0, 0.0, -1.0, 0.0, 0.0, -0.5, 0.0, s],
            ),
        ] {
            close(encode_sn3d(direction), expected);
        }
    }

    #[test]
    fn steam_east_becomes_listener_right_with_order_two_and_pitch() {
        let listener = Pose {
            position: EnuVector3::default(),
            forward: EnuVector3::new(0.0, 1.0, 0.0),
            up: EnuVector3::new(0.0, 0.0, 1.0),
        };
        // Native Google SH has a negative Y(1,-1), hence east is positive.
        let native = [
            0.282_095, 0.488_603, 0.0, 0.0, 0.0, 0.0, -0.315_392, 0.0, -0.546_274,
        ];
        let transform = ListenerTransform::new(
            listener,
            SpatialEnvironmentalBasis::RightHandedXRightYUpZBack,
        )
        .unwrap();
        let rotated = transform
            .environment
            .map(|row| row.into_iter().zip(native).map(|(a, b)| a * b).sum());
        close(rotated, encode_sn3d([0.0, -1.0, 0.0]));

        let direction = normalized([0.3, 0.4, 0.5]).unwrap();
        for basis in [
            SpatialEnvironmentalBasis::RightHandedEnu,
            SpatialEnvironmentalBasis::RightHandedXRightYUpZBack,
        ] {
            let tilted = ListenerTransform::new(
                Pose {
                    forward: EnuVector3::new(0.0, 1.0, 1.0),
                    up: EnuVector3::new(0.0, -1.0, 1.0),
                    ..listener
                },
                basis,
            )
            .unwrap();
            let source_axes = match basis {
                SpatialEnvironmentalBasis::RightHandedEnu => direction,
                SpatialEnvironmentalBasis::RightHandedXRightYUpZBack => {
                    [direction[1], -direction[0], direction[2]]
                }
            };
            let sn3d = encode_sn3d(source_axes);
            let native: [f32; CHANNELS] = std::array::from_fn(|channel| {
                let order = if channel == 0 {
                    0
                } else if channel < 4 {
                    1
                } else {
                    2
                };
                let mut coefficient = sn3d[channel] * ((2 * order + 1) as f32).sqrt();
                if basis == SpatialEnvironmentalBasis::RightHandedXRightYUpZBack {
                    coefficient /= SQRT_4_PI;
                    if matches!(channel, 1 | 3 | 5 | 7) {
                        coefficient = -coefficient;
                    }
                }
                coefficient
            });
            let rotated = tilted
                .environment
                .map(|row| row.into_iter().zip(native).map(|(a, b)| a * b).sum());
            close(rotated, encode_sn3d(tilted.direction(direction)));
        }
    }

    #[test]
    fn extensible_float_header_and_hot_sample_rejection() {
        let header = wav_header(9 * 4 * 16, 16);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(header[4..8].try_into().unwrap()),
            72 + 9 * 4 * 16
        );
        assert_eq!(&header[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(header[16..20].try_into().unwrap()), 40);
        assert_eq!(
            u16::from_le_bytes(header[20..22].try_into().unwrap()),
            0xfffe
        );
        assert_eq!(u16::from_le_bytes(header[22..24].try_into().unwrap()), 9);
        assert_eq!(
            u32::from_le_bytes(header[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(
            u32::from_le_bytes(header[28..32].try_into().unwrap()),
            1_728_000
        );
        assert_eq!(u16::from_le_bytes(header[32..34].try_into().unwrap()), 36);
        assert_eq!(u16::from_le_bytes(header[34..36].try_into().unwrap()), 32);
        assert_eq!(u16::from_le_bytes(header[36..38].try_into().unwrap()), 22);
        assert_eq!(u16::from_le_bytes(header[38..40].try_into().unwrap()), 32);
        assert_eq!(u32::from_le_bytes(header[40..44].try_into().unwrap()), 0);
        assert_eq!(
            &header[44..60],
            &[3, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113]
        );
        assert_eq!(&header[60..64], b"fact");
        assert_eq!(u32::from_le_bytes(header[68..72].try_into().unwrap()), 16);
        assert_eq!(&header[72..76], b"data");
        assert_eq!(u32::from_le_bytes(header[76..80].try_into().unwrap()), 576);
        let forbidden = Path::new("/path-that-must-never-be-opened/ambix.wav");
        assert!(
            write_ambix_wav(forbidden, &[1.0; CHANNELS])
                .unwrap_err()
                .contains("-1 dBFS")
        );
        assert!(
            write_ambix_wav(forbidden, &[f32::NAN; CHANNELS])
                .unwrap_err()
                .contains("non-finite")
        );
    }
}
