//! Frozen backend-private representation of Steam Audio environmental fields.
//!
//! Steam Audio 4.8.1 emits both an unspatialized `IPLPathEffect` and CPU
//! reflection audio in its native Ambisonics convention. The pinned header
//! defines that convention as ACN channel order and N3D normalization. Its
//! world coordinates are right-handed, with +X right, +Y up, and +Z back
//! (`-Z` is ahead). That is already the neutral representation frozen here;
//! raw Steam planes therefore cross this seam through an identity transform.
//!
//! The identity is source-audited against Steam Audio tag `v4.8.1`
//! (`0da1825`): `core/src/core/path_effect.cpp` copies active SH coefficients
//! directly into output planes when `spatialize == false`, while
//! `core/src/core/audio_buffer.cpp` leaves ACN/N3D unchanged. Reflection
//! channel enumeration follows the same nested `(l, m)` traversal in
//! `core/src/core/reflection_simulator.cpp` and its inactive output planes are
//! cleared by `core/src/core/overlap_save_convolution_effect.cpp`.

/// Highest environmental Ambisonic order admitted by the frozen v1 seam.
pub(crate) const MAX_NEUTRAL_ENVIRONMENT_ORDER: i32 = 2;
/// Number of ACN planes in an order-two environmental field.
pub(crate) const MAX_NEUTRAL_ENVIRONMENT_CHANNELS: usize = 9;
/// Active ACN prefix length for orders zero, one, and two.
pub(crate) const NEUTRAL_ENVIRONMENT_CHANNEL_COUNTS: [usize; 3] = [1, 4, 9];

/// Raw Steam channel index selected for each neutral ACN plane.
///
/// Keeping this explicit prevents a later adapter from silently applying a
/// FuMa reorder or confusing Steam's axes with the engine's ENU axes.
pub(crate) const STEAM_TO_NEUTRAL_ACN_INDICES: [usize; MAX_NEUTRAL_ENVIRONMENT_CHANNELS] =
    [0, 1, 2, 3, 4, 5, 6, 7, 8];

/// Per-plane normalization gain from Steam native N3D to neutral N3D.
pub(crate) const STEAM_TO_NEUTRAL_N3D_GAINS: [f32; MAX_NEUTRAL_ENVIRONMENT_CHANNELS] =
    [1.0; MAX_NEUTRAL_ENVIRONMENT_CHANNELS];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeutralEnvironmentWorldBasis {
    /// Steam Audio right-handed coordinates: +X right, +Y up, +Z back.
    SteamRightHandedXRightYUpZBack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeutralEnvironmentChannelOrder {
    Acn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeutralEnvironmentNormalization {
    N3d,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NeutralEnvironmentLayout {
    pub world_basis: NeutralEnvironmentWorldBasis,
    pub channel_order: NeutralEnvironmentChannelOrder,
    pub normalization: NeutralEnvironmentNormalization,
    pub max_order: i32,
    pub max_channels: usize,
}

/// Frozen metadata attached to both raw path and reflection planes.
pub(crate) const NEUTRAL_ENVIRONMENT_LAYOUT: NeutralEnvironmentLayout = NeutralEnvironmentLayout {
    world_basis: NeutralEnvironmentWorldBasis::SteamRightHandedXRightYUpZBack,
    channel_order: NeutralEnvironmentChannelOrder::Acn,
    normalization: NeutralEnvironmentNormalization::N3d,
    max_order: MAX_NEUTRAL_ENVIRONMENT_ORDER,
    max_channels: MAX_NEUTRAL_ENVIRONMENT_CHANNELS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeutralEnvironmentError {
    UnsupportedOrder { order: i32 },
    NonFiniteActivePlane { channel: usize },
}

/// Returns the active ACN prefix length for a frozen environmental order.
pub(crate) const fn active_channel_count(order: i32) -> Result<usize, NeutralEnvironmentError> {
    match order {
        0 => Ok(1),
        1 => Ok(4),
        2 => Ok(9),
        _ => Err(NeutralEnvironmentError::UnsupportedOrder { order }),
    }
}

/// Copies one raw Steam environmental frame into the frozen neutral layout.
///
/// The operation is allocation-free and byte-preserving for every active
/// finite plane. Output is cleared before validation, so an invalid frame can
/// never expose a partial field. Planes outside the selected order are not
/// semantically active: stale or non-finite input there is ignored and the
/// corresponding neutral planes remain positive zero.
pub(crate) fn steam_environment_frame_to_neutral(
    order: i32,
    steam_planes: &[f32; MAX_NEUTRAL_ENVIRONMENT_CHANNELS],
    neutral_planes: &mut [f32; MAX_NEUTRAL_ENVIRONMENT_CHANNELS],
) -> Result<(), NeutralEnvironmentError> {
    neutral_planes.fill(0.0);
    let active_channels = active_channel_count(order)?;

    for (channel, sample) in steam_planes[..active_channels].iter().copied().enumerate() {
        if !sample.is_finite() {
            return Err(NeutralEnvironmentError::NonFiniteActivePlane { channel });
        }
    }

    // The audited Steam-to-neutral transform is I9. Copying the prefix rather
    // than multiplying by a generic matrix preserves every finite f32 bit.
    neutral_planes[..active_channels].copy_from_slice(&steam_planes[..active_channels]);
    Ok(())
}
