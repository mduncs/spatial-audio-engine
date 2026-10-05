//! Trigger-time planning for pre-declared supersonic shot source pairs.
//!
//! This sidecar owns no clock and creates no backend source. A host constructs
//! the crack and blast slots with its retained world, evaluates one plan at a
//! trigger boundary, teleports both slots in one `SimulationUpdate`, and puts
//! the returned leading silence into their dry stems. The renderer's existing
//! source-distance delay then completes each ballistic arrival time.

use fightbox_api::ballistics::{
    BallisticMachSegment, MachConeTangent, PiecewiseMachConeTangent, crack_spl_at_one_meter_db,
    listener_receives_crack, n_wave_crest_factor_db, n_wave_duration_ms,
    n_wave_duration_ms_from_reference, solve_mach_cone_tangent, solve_piecewise_mach_cone_tangent,
    whitham_level_offset_db,
};
use fightbox_api::{EnuVector3, ImpulseClass};

/// Fixed workbench event program length.
///
/// Three seconds matches the governor's transient-protection window and is
/// long enough for the signed two-second artillery crop plus city-scale flight
/// time. Longer reflection tails remain an engine-stage concern.
pub const EVENT_PROGRAM_SECONDS: f64 = 3.0;

/// Fixture-owned level anchors for one ballistic event family.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticEventLevels {
    /// Muzzle-blast source power in the engine's calibrated scene vocabulary.
    pub blast_spl_at_one_meter_db: f64,
    /// Received crack offset over this shot's free-field blast at the 30 m
    /// Whitham reference distance. The signed Wave 12 value is `+3 dB`.
    pub crack_over_blast_db_at_reference: f64,
}

/// Fixture-owned straight, constant-Mach shot declaration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticShot {
    pub muzzle_position_enu: EnuVector3,
    pub direction_enu: EnuVector3,
    pub mach: f64,
    pub levels: BallisticEventLevels,
}

/// Fixture-owned finite straight trajectory with piecewise-constant speed.
///
/// The slice is read only while planning. It may include positive subsonic
/// legs so the projectile endpoint clock remains complete after the shock path
/// ends. Such legs never emit a crack candidate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PiecewiseBallisticShot<'a> {
    pub muzzle_position_enu: EnuVector3,
    pub direction_enu: EnuVector3,
    pub segments: &'a [BallisticMachSegment],
    pub levels: BallisticEventLevels,
}

/// One reusable event slot's trigger-time values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticEventSource {
    pub position_enu: EnuVector3,
    pub spl_at_one_meter_db: f64,
    pub impulse_class: ImpulseClass,
    /// Silence generated into the dry stem before its first physical sample.
    pub embedded_leading_silence_s: f64,
    /// The propagation delay already applied by the retained render graph.
    pub engine_propagation_delay_s: f64,
    /// Sum of the two independently owned timing terms above.
    pub arrival_time_s: f64,
}

/// Complete trigger result for one pre-declared crack/blast pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticShotPlan {
    pub tangent: MachConeTangent,
    pub miss_distance_m: f64,
    pub closest_approach_m: f64,
    /// Unit wave-travel direction from the tangent point to the listener.
    pub crack_arrival_direction_enu: EnuVector3,
    /// Unit wave-travel direction from the muzzle to the listener.
    pub blast_arrival_direction_enu: EnuVector3,
    pub crack: Option<BallisticEventSource>,
    pub blast: BallisticEventSource,
    pub n_wave_duration_ms: f64,
    pub whitham_level_offset_db: f64,
}

/// Terminal trajectory timing for a separately authored impact or endpoint.
///
/// This is scheduling metadata, not an impact event: the ballistic planner has
/// no impact asset or level authority. The generic event queue may later map
/// it to one atomic crack/blast/impact admission.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticTrajectoryEnd {
    pub position_enu: EnuVector3,
    pub distance_from_muzzle_m: f64,
    pub projectile_time_s: f64,
    pub acoustic_distance_m: f64,
    pub engine_propagation_delay_s: f64,
    pub arrival_time_s: f64,
    /// Unit wave-travel direction from the endpoint to the listener. This is
    /// `None` when both positions coincide and no direction exists.
    pub arrival_direction_enu: Option<EnuVector3>,
}

/// Trigger result for one finite straight piecewise-Mach trajectory.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PiecewiseBallisticShotPlan {
    pub tangent: Option<PiecewiseMachConeTangent>,
    pub miss_distance_m: f64,
    pub closest_approach_m: f64,
    /// Unit wave-travel direction from the selected emission point to the
    /// listener. It is absent when the finite path admits no crack.
    pub crack_arrival_direction_enu: Option<EnuVector3>,
    /// Unit wave-travel direction from the muzzle to the listener.
    pub blast_arrival_direction_enu: EnuVector3,
    pub crack: Option<BallisticEventSource>,
    pub blast: BallisticEventSource,
    pub trajectory_end: BallisticTrajectoryEnd,
    pub n_wave_duration_ms: f64,
    pub whitham_level_offset_db: f64,
}

/// Optional projectile-scale replacements for the signed small-arms anchors.
///
/// `Default` leaves every field `None` and reproduces
/// [`plan_piecewise_ballistic_shot`] bit for bit.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BallisticPlanOverrides {
    /// N-wave duration at the 30 m Whitham reference miss distance, in
    /// milliseconds. It scales the same `b^(1/4)` law as the signed anchor.
    pub n_wave_reference_duration_ms: Option<f64>,
    /// Received crack *peak* level at the 30 m Whitham reference miss distance.
    ///
    /// When present, the crack level is `anchor + whitham_level_offset_db(b)`
    /// at the listener, independent of any muzzle blast or of where the
    /// declared trajectory starts. The planner converts it to the renderer's
    /// RMS reference-level convention with [`n_wave_crest_factor_db`].
    pub crack_peak_db_at_reference: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BallisticEventError {
    NonFiniteInput,
    DegenerateDirection,
    NonSupersonicMach,
    EmptyTrajectory,
    InvalidSegmentLength,
    InvalidSegmentMach,
    InvalidSampleRate,
    InvalidProgramLength,
    InvalidOverride,
}

/// Evaluates a shot against the listener position captured at the trigger.
///
/// The scalar cone solution, directions, level law, and N-wave duration all
/// come from `fightbox_api::ballistics`. This module only lifts its signed
/// listener-local result into arbitrary ENU geometry and assigns renderer
/// responsibilities to pre-declared source slots.
pub fn plan_ballistic_shot(
    shot: BallisticShot,
    listener_position_enu: EnuVector3,
) -> Result<BallisticShotPlan, BallisticEventError> {
    if !finite_vector(shot.muzzle_position_enu)
        || !finite_vector(shot.direction_enu)
        || !finite_vector(listener_position_enu)
        || !shot.mach.is_finite()
        || !shot.levels.blast_spl_at_one_meter_db.is_finite()
        || !shot.levels.crack_over_blast_db_at_reference.is_finite()
    {
        return Err(BallisticEventError::NonFiniteInput);
    }
    if shot.mach <= 1.0 {
        return Err(BallisticEventError::NonSupersonicMach);
    }
    let direction =
        normalized(shot.direction_enu).ok_or(BallisticEventError::DegenerateDirection)?;
    let listener_offset = subtract(listener_position_enu, shot.muzzle_position_enu);
    let closest_approach_m = f64::from(dot(listener_offset, direction));
    let perpendicular = subtract(listener_offset, scale(direction, closest_approach_m as f32));
    let miss_distance_m = f64::from(length(perpendicular));
    let miss_direction = normalized(perpendicular).unwrap_or_else(|| orthogonal_unit(direction));

    // The frozen ballistics module is the only source for these cone scalars
    // and signed local direction components.
    let tangent = solve_mach_cone_tangent(shot.mach, miss_distance_m, closest_approach_m);
    if tangent.blast_distance_m <= 0.0 {
        return Err(BallisticEventError::DegenerateDirection);
    }
    let crack_arrival_direction_enu =
        combine_local_direction(direction, miss_direction, tangent.crack_direction_listener);
    let blast_arrival_direction_enu =
        combine_local_direction(direction, miss_direction, tangent.blast_direction_listener);

    let blast = BallisticEventSource {
        position_enu: shot.muzzle_position_enu,
        spl_at_one_meter_db: shot.levels.blast_spl_at_one_meter_db,
        impulse_class: ImpulseClass::ArtilleryThunder,
        // The muzzle emits at the trigger epoch. Its entire arrival is the
        // ordinary source-to-listener delay already owned by the engine.
        embedded_leading_silence_s: 0.0,
        engine_propagation_delay_s: tangent.blast_arrival_time_s,
        arrival_time_s: tangent.blast_arrival_time_s,
    };

    // At exactly zero miss distance the tangent source collapses onto the
    // listener and the inverse-distance calibration is singular. Treat that
    // degenerate axis case as blast-only rather than publishing infinities.
    let receives_crack = miss_distance_m > 0.0
        && listener_receives_crack(shot.mach, miss_distance_m, closest_approach_m);
    let (duration_ms, level_offset_db) = if miss_distance_m > 0.0 {
        (
            n_wave_duration_ms(miss_distance_m),
            whitham_level_offset_db(miss_distance_m),
        )
    } else {
        (0.0, 0.0)
    };
    let crack = receives_crack.then(|| {
        let blast_received_db =
            shot.levels.blast_spl_at_one_meter_db - 20.0 * tangent.blast_distance_m.log10();
        let spl_at_one_meter_db = crack_spl_at_one_meter_db(
            blast_received_db,
            shot.levels.crack_over_blast_db_at_reference + level_offset_db,
            tangent.r_star_m,
        );
        BallisticEventSource {
            // Arrival direction points source -> listener, so the virtual
            // tangent source lies one r* in the opposite direction.
            position_enu: subtract(
                listener_position_enu,
                scale(crack_arrival_direction_enu, tangent.r_star_m as f32),
            ),
            spl_at_one_meter_db,
            impulse_class: ImpulseClass::None,
            // This replaces the forbidden activation-time scheduler. The
            // engine contributes only r*/c; the stem contributes t*=s*/Mc.
            embedded_leading_silence_s: tangent.t_star_s,
            engine_propagation_delay_s: tangent.r_star_m
                / fightbox_api::ballistics::SOUND_SPEED_MPS,
            arrival_time_s: tangent.crack_arrival_time_s,
        }
    });

    Ok(BallisticShotPlan {
        tangent,
        miss_distance_m,
        closest_approach_m,
        crack_arrival_direction_enu,
        blast_arrival_direction_enu,
        crack,
        blast,
        n_wave_duration_ms: duration_ms,
        whitham_level_offset_db: level_offset_db,
    })
}

/// Evaluates a finite straight piecewise-Mach trajectory at its trigger epoch.
///
/// The muzzle blast is independent of projectile flight and is always planned
/// from muzzle to listener. A crack exists only when the continuous finite
/// trajectory admits a tangent within a supersonic leg (or at a continuing
/// supersonic deceleration join). The returned endpoint clock lets a later
/// queue adapter schedule a separately authored impact without extending the
/// shock path past the projectile's declared end.
pub fn plan_piecewise_ballistic_shot(
    shot: PiecewiseBallisticShot<'_>,
    listener_position_enu: EnuVector3,
) -> Result<PiecewiseBallisticShotPlan, BallisticEventError> {
    plan_piecewise_ballistic_shot_with(
        shot,
        listener_position_enu,
        BallisticPlanOverrides::default(),
    )
}

/// [`plan_piecewise_ballistic_shot`] with optional projectile-scale anchors.
///
/// Geometry and timing are unchanged by `overrides`; only the N-wave duration
/// and, for an explicit peak anchor, the crack reference level differ.
pub fn plan_piecewise_ballistic_shot_with(
    shot: PiecewiseBallisticShot<'_>,
    listener_position_enu: EnuVector3,
    overrides: BallisticPlanOverrides,
) -> Result<PiecewiseBallisticShotPlan, BallisticEventError> {
    if overrides
        .n_wave_reference_duration_ms
        .is_some_and(|duration_ms| !duration_ms.is_finite() || duration_ms <= 0.0)
        || overrides
            .crack_peak_db_at_reference
            .is_some_and(|level_db| !level_db.is_finite())
    {
        return Err(BallisticEventError::InvalidOverride);
    }
    if !finite_vector(shot.muzzle_position_enu)
        || !finite_vector(shot.direction_enu)
        || !finite_vector(listener_position_enu)
        || !shot.levels.blast_spl_at_one_meter_db.is_finite()
        || !shot.levels.crack_over_blast_db_at_reference.is_finite()
    {
        return Err(BallisticEventError::NonFiniteInput);
    }
    if shot.segments.is_empty() {
        return Err(BallisticEventError::EmptyTrajectory);
    }
    for segment in shot.segments {
        if !segment.length_m.is_finite() || !segment.mach.is_finite() {
            return Err(BallisticEventError::NonFiniteInput);
        }
        if segment.length_m <= 0.0 {
            return Err(BallisticEventError::InvalidSegmentLength);
        }
        if segment.mach <= 0.0 {
            return Err(BallisticEventError::InvalidSegmentMach);
        }
    }

    let direction =
        normalized(shot.direction_enu).ok_or(BallisticEventError::DegenerateDirection)?;
    let listener_offset = subtract(listener_position_enu, shot.muzzle_position_enu);
    let closest_approach_m = f64::from(dot(listener_offset, direction));
    let perpendicular = subtract(listener_offset, scale(direction, closest_approach_m as f32));
    let miss_distance_m = f64::from(length(perpendicular));
    let miss_direction = normalized(perpendicular).unwrap_or_else(|| orthogonal_unit(direction));
    let blast_distance_m = closest_approach_m.hypot(miss_distance_m);
    if !blast_distance_m.is_finite() {
        return Err(BallisticEventError::NonFiniteInput);
    }
    if blast_distance_m <= 0.0 {
        return Err(BallisticEventError::DegenerateDirection);
    }

    let blast_arrival_time_s = blast_distance_m / fightbox_api::ballistics::SOUND_SPEED_MPS;
    let blast_arrival_direction_enu = combine_local_direction(
        direction,
        miss_direction,
        [
            0.0,
            miss_distance_m / blast_distance_m,
            closest_approach_m / blast_distance_m,
        ],
    );
    let blast = BallisticEventSource {
        position_enu: shot.muzzle_position_enu,
        spl_at_one_meter_db: shot.levels.blast_spl_at_one_meter_db,
        impulse_class: ImpulseClass::ArtilleryThunder,
        embedded_leading_silence_s: 0.0,
        engine_propagation_delay_s: blast_arrival_time_s,
        arrival_time_s: blast_arrival_time_s,
    };

    let tangent =
        solve_piecewise_mach_cone_tangent(shot.segments, miss_distance_m, closest_approach_m);
    let (duration_ms, level_offset_db) = if miss_distance_m > 0.0 {
        (
            overrides.n_wave_reference_duration_ms.map_or_else(
                || n_wave_duration_ms(miss_distance_m),
                |reference_ms| n_wave_duration_ms_from_reference(reference_ms, miss_distance_m),
            ),
            whitham_level_offset_db(miss_distance_m),
        )
    } else {
        (0.0, 0.0)
    };
    if !duration_ms.is_finite() || !level_offset_db.is_finite() {
        return Err(BallisticEventError::NonFiniteInput);
    }

    let (crack_arrival_direction_enu, crack) = if let Some(candidate) = tangent {
        let arrival_direction = combine_local_direction(
            direction,
            miss_direction,
            candidate.crack_direction_listener,
        );
        let virtual_distance_m = candidate.acoustic_distance_m as f32;
        if !virtual_distance_m.is_finite() {
            return Err(BallisticEventError::NonFiniteInput);
        }
        let virtual_position = subtract(
            listener_position_enu,
            scale(arrival_direction, virtual_distance_m),
        );
        let spl_at_one_meter_db = if let Some(peak_db) = overrides.crack_peak_db_at_reference {
            // Whitham peak pressure depends on the perpendicular miss alone;
            // the acoustic distance only inverts the renderer's one gain.
            crack_spl_at_one_meter_db(
                peak_db - n_wave_crest_factor_db(),
                level_offset_db,
                candidate.acoustic_distance_m,
            )
        } else {
            let blast_received_db =
                shot.levels.blast_spl_at_one_meter_db - 20.0 * blast_distance_m.log10();
            crack_spl_at_one_meter_db(
                blast_received_db,
                shot.levels.crack_over_blast_db_at_reference + level_offset_db,
                candidate.acoustic_distance_m,
            )
        };
        if !finite_vector(virtual_position) || !spl_at_one_meter_db.is_finite() {
            return Err(BallisticEventError::NonFiniteInput);
        }
        (
            Some(arrival_direction),
            Some(BallisticEventSource {
                position_enu: virtual_position,
                spl_at_one_meter_db,
                impulse_class: ImpulseClass::None,
                embedded_leading_silence_s: candidate.emission_time_s,
                engine_propagation_delay_s: candidate.acoustic_distance_m
                    / fightbox_api::ballistics::SOUND_SPEED_MPS,
                arrival_time_s: candidate.arrival_time_s,
            }),
        )
    } else {
        (None, None)
    };

    let mut distance_from_muzzle_m = 0.0_f64;
    let mut projectile_time_s = 0.0_f64;
    for segment in shot.segments {
        distance_from_muzzle_m += segment.length_m;
        projectile_time_s +=
            segment.length_m / (segment.mach * fightbox_api::ballistics::SOUND_SPEED_MPS);
        if !distance_from_muzzle_m.is_finite() || !projectile_time_s.is_finite() {
            return Err(BallisticEventError::NonFiniteInput);
        }
    }
    let endpoint_distance_m = distance_from_muzzle_m as f32;
    if !endpoint_distance_m.is_finite() {
        return Err(BallisticEventError::NonFiniteInput);
    }
    let endpoint_position_enu = add(
        shot.muzzle_position_enu,
        scale(direction, endpoint_distance_m),
    );
    if !finite_vector(endpoint_position_enu) {
        return Err(BallisticEventError::NonFiniteInput);
    }
    let endpoint_to_listener = subtract(listener_position_enu, endpoint_position_enu);
    let endpoint_acoustic_distance_m = f64::from(length(endpoint_to_listener));
    if !endpoint_acoustic_distance_m.is_finite() {
        return Err(BallisticEventError::NonFiniteInput);
    }
    let endpoint_propagation_delay_s =
        endpoint_acoustic_distance_m / fightbox_api::ballistics::SOUND_SPEED_MPS;
    let endpoint_arrival_time_s = projectile_time_s + endpoint_propagation_delay_s;
    if !endpoint_arrival_time_s.is_finite() {
        return Err(BallisticEventError::NonFiniteInput);
    }
    let trajectory_end = BallisticTrajectoryEnd {
        position_enu: endpoint_position_enu,
        distance_from_muzzle_m,
        projectile_time_s,
        acoustic_distance_m: endpoint_acoustic_distance_m,
        engine_propagation_delay_s: endpoint_propagation_delay_s,
        arrival_time_s: endpoint_arrival_time_s,
        arrival_direction_enu: normalized(endpoint_to_listener),
    };

    Ok(PiecewiseBallisticShotPlan {
        tangent,
        miss_distance_m,
        closest_approach_m,
        crack_arrival_direction_enu,
        blast_arrival_direction_enu,
        crack,
        blast,
        trajectory_end,
        n_wave_duration_ms: duration_ms,
        whitham_level_offset_db: level_offset_db,
    })
}

/// Synthesizes the signed N-wave inside a fixed-length dry stem.
///
/// The event source drive consumes the returned `audible_program_rms_dbfs` as
/// its asset analysis. Silence is intentionally excluded from that measurement:
/// it is transport timing, not source power. No makeup gain is applied here.
pub fn synthesize_crack_stem(
    plan: &BallisticShotPlan,
    sample_rate_hz: u32,
    program_frames: usize,
) -> Result<(Vec<f32>, f32), BallisticEventError> {
    synthesize_crack_program(
        plan.crack,
        plan.n_wave_duration_ms,
        sample_rate_hz,
        program_frames,
    )
}

/// Synthesizes the selected finite-trajectory crack into a fixed dry stem.
///
/// A no-crack plan produces deterministic silence and the same sentinel
/// analysis value as the legacy constant-Mach planner.
pub fn synthesize_piecewise_crack_stem(
    plan: &PiecewiseBallisticShotPlan,
    sample_rate_hz: u32,
    program_frames: usize,
) -> Result<(Vec<f32>, f32), BallisticEventError> {
    synthesize_crack_program(
        plan.crack,
        plan.n_wave_duration_ms,
        sample_rate_hz,
        program_frames,
    )
}

fn synthesize_crack_program(
    crack: Option<BallisticEventSource>,
    duration_ms: f64,
    sample_rate_hz: u32,
    program_frames: usize,
) -> Result<(Vec<f32>, f32), BallisticEventError> {
    if sample_rate_hz == 0 {
        return Err(BallisticEventError::InvalidSampleRate);
    }
    let Some(crack) = crack else {
        return Ok((vec![0.0; program_frames], -120.0));
    };
    let leading_frames = seconds_to_frames(crack.embedded_leading_silence_s, sample_rate_hz);
    if leading_frames.saturating_add(n_wave_frames(duration_ms, sample_rate_hz)) > program_frames {
        return Err(BallisticEventError::InvalidProgramLength);
    }
    let mut stem = vec![0.0; program_frames];
    let rms_dbfs = synthesize_n_wave_into(duration_ms, sample_rate_hz, leading_frames, &mut stem)?;
    Ok((stem, rms_dbfs))
}

/// Number of frames the signed N-wave occupies at `duration_ms`.
#[must_use]
pub fn n_wave_frames(duration_ms: f64, sample_rate_hz: u32) -> usize {
    ((duration_ms / 1_000.0) * f64::from(sample_rate_hz))
        .round()
        .max(4.0) as usize
}

/// Writes one signed N-wave into caller-owned storage without allocating.
///
/// `output` is cleared, the wave starts at `leading_frames`, and the return
/// value is the wave-only RMS in dBFS (leading silence is transport timing,
/// not source power). The first wave sample is the `+1.0` peak.
pub fn synthesize_n_wave_into(
    duration_ms: f64,
    sample_rate_hz: u32,
    leading_frames: usize,
    output: &mut [f32],
) -> Result<f32, BallisticEventError> {
    if sample_rate_hz == 0 {
        return Err(BallisticEventError::InvalidSampleRate);
    }
    if !duration_ms.is_finite() || duration_ms <= 0.0 {
        return Err(BallisticEventError::NonFiniteInput);
    }
    let wave_frames = n_wave_frames(duration_ms, sample_rate_hz);
    if leading_frames.saturating_add(wave_frames) > output.len() {
        return Err(BallisticEventError::InvalidProgramLength);
    }
    output.fill(0.0);
    let wave = &mut output[leading_frames..leading_frames + wave_frames];
    for (frame, sample) in wave.iter_mut().enumerate() {
        let phase = frame as f64 / (wave_frames - 1) as f64;
        *sample = if phase <= fightbox_api::ballistics::N_WAVE_POSITIVE_FRACTION {
            (1.0 - phase / fightbox_api::ballistics::N_WAVE_POSITIVE_FRACTION) as f32
        } else {
            (fightbox_api::ballistics::N_WAVE_NEGATIVE_PEAK
                * (phase - fightbox_api::ballistics::N_WAVE_POSITIVE_FRACTION)
                / fightbox_api::ballistics::N_WAVE_NEGATIVE_FRACTION) as f32
        };
    }
    let mean_square = wave
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / wave.len() as f64;
    Ok((10.0 * mean_square.log10()) as f32)
}

fn seconds_to_frames(seconds: f64, sample_rate_hz: u32) -> usize {
    (seconds * f64::from(sample_rate_hz)).round().max(0.0) as usize
}

fn finite_vector(vector: EnuVector3) -> bool {
    vector.east_m.is_finite() && vector.north_m.is_finite() && vector.up_m.is_finite()
}

fn dot(left: EnuVector3, right: EnuVector3) -> f32 {
    left.east_m * right.east_m + left.north_m * right.north_m + left.up_m * right.up_m
}

fn length(vector: EnuVector3) -> f32 {
    dot(vector, vector).sqrt()
}

fn normalized(vector: EnuVector3) -> Option<EnuVector3> {
    let magnitude = length(vector);
    (magnitude.is_finite() && magnitude > 1.0e-6).then(|| scale(vector, magnitude.recip()))
}

fn scale(vector: EnuVector3, scale: f32) -> EnuVector3 {
    EnuVector3::new(
        vector.east_m * scale,
        vector.north_m * scale,
        vector.up_m * scale,
    )
}

fn subtract(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m - right.east_m,
        left.north_m - right.north_m,
        left.up_m - right.up_m,
    )
}

fn add(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m + right.east_m,
        left.north_m + right.north_m,
        left.up_m + right.up_m,
    )
}

fn combine_local_direction(
    trajectory: EnuVector3,
    miss: EnuVector3,
    local: [f64; 3],
) -> EnuVector3 {
    let combined = EnuVector3::new(
        trajectory.east_m * local[2] as f32 + miss.east_m * local[1] as f32,
        trajectory.north_m * local[2] as f32 + miss.north_m * local[1] as f32,
        trajectory.up_m * local[2] as f32 + miss.up_m * local[1] as f32,
    );
    normalized(combined).expect("ballistics returns a unit local direction")
}

fn orthogonal_unit(direction: EnuVector3) -> EnuVector3 {
    let candidate = if direction.up_m.abs() < 0.9 {
        EnuVector3::new(-direction.north_m, direction.east_m, 0.0)
    } else {
        EnuVector3::new(0.0, -direction.up_m, direction.north_m)
    };
    normalized(candidate).expect("a finite unit vector has an orthogonal axis")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_shot() -> BallisticShot {
        BallisticShot {
            muzzle_position_enu: EnuVector3::default(),
            direction_enu: EnuVector3::new(0.0, 1.0, 0.0),
            mach: 2.5,
            levels: BallisticEventLevels {
                blast_spl_at_one_meter_db: 155.0,
                crack_over_blast_db_at_reference: 3.0,
            },
        }
    }

    #[test]
    fn worked_example_decomposes_to_the_frozen_arrivals() {
        let plan = plan_ballistic_shot(signed_shot(), EnuVector3::new(0.0, 60.0, 30.0)).unwrap();
        let crack = plan.crack.unwrap();
        assert_eq!(
            format!("{:.4}", crack.embedded_leading_silence_s * 1_000.0),
            "54.7020"
        );
        assert_eq!(
            format!("{:.4}", crack.engine_propagation_delay_s * 1_000.0),
            "95.4306"
        );
        assert_eq!(format!("{:.4}", crack.arrival_time_s * 1_000.0), "150.1325");
        assert_eq!(
            format!("{:.4}", plan.blast.arrival_time_s * 1_000.0),
            "195.5745"
        );
        assert_eq!(
            format!("{:.4}", plan.tangent.lead_time_s * 1_000.0),
            "45.4419"
        );
        assert_eq!(crack.impulse_class, ImpulseClass::None);
        assert_eq!(plan.blast.impulse_class, ImpulseClass::ArtilleryThunder);
    }

    #[test]
    fn n_wave_keeps_the_signed_shape_after_embedded_t_star_silence() {
        let plan = plan_ballistic_shot(signed_shot(), EnuVector3::new(0.0, 60.0, 30.0)).unwrap();
        let (stem, audible_rms_dbfs) = synthesize_crack_stem(&plan, 48_000, 144_000).unwrap();
        let onset = stem.iter().position(|sample| *sample != 0.0).unwrap();
        assert_eq!(onset, (plan.tangent.t_star_s * 48_000.0).round() as usize);
        assert_eq!(stem[onset].to_bits(), 1.0_f32.to_bits());
        let minimum = stem.iter().copied().fold(0.0_f32, f32::min);
        assert!((minimum - fightbox_api::ballistics::N_WAVE_NEGATIVE_PEAK as f32).abs() < 0.03);
        assert!(audible_rms_dbfs.is_finite() && audible_rms_dbfs < 0.0);
    }

    #[test]
    fn rejected_listener_has_only_the_predeclared_blast_slot() {
        let plan = plan_ballistic_shot(signed_shot(), EnuVector3::new(0.0, -30.0, 30.0)).unwrap();
        assert!(plan.crack.is_none());
        assert_eq!(
            format!("{:.4}", plan.blast.arrival_time_s * 1_000.0),
            "123.6921"
        );
    }

    #[test]
    fn piecewise_single_segment_preserves_legacy_crack_blast_and_stem() {
        let legacy = plan_ballistic_shot(signed_shot(), EnuVector3::new(0.0, 60.0, 30.0)).unwrap();
        let segments = [BallisticMachSegment {
            length_m: 500.0,
            mach: 2.5,
        }];
        let finite = plan_piecewise_ballistic_shot(
            PiecewiseBallisticShot {
                muzzle_position_enu: EnuVector3::default(),
                direction_enu: EnuVector3::new(0.0, 1.0, 0.0),
                segments: &segments,
                levels: signed_shot().levels,
            },
            EnuVector3::new(0.0, 60.0, 30.0),
        )
        .unwrap();
        let tangent = finite.tangent.unwrap();

        assert_eq!(
            tangent.emission_distance_m.to_bits(),
            legacy.tangent.s_star_m.to_bits()
        );
        assert_eq!(
            tangent.emission_time_s.to_bits(),
            legacy.tangent.t_star_s.to_bits()
        );
        assert_eq!(
            tangent.acoustic_distance_m.to_bits(),
            legacy.tangent.r_star_m.to_bits()
        );
        assert_eq!(
            tangent.arrival_time_s.to_bits(),
            legacy.tangent.crack_arrival_time_s.to_bits()
        );
        assert_eq!(finite.crack, legacy.crack);
        assert_eq!(finite.blast, legacy.blast);
        assert_eq!(
            finite.crack_arrival_direction_enu,
            Some(legacy.crack_arrival_direction_enu)
        );
        assert_eq!(
            finite.blast_arrival_direction_enu,
            legacy.blast_arrival_direction_enu
        );
        assert_eq!(finite.n_wave_duration_ms, legacy.n_wave_duration_ms);
        assert_eq!(
            finite.whitham_level_offset_db,
            legacy.whitham_level_offset_db
        );

        let legacy_stem = synthesize_crack_stem(&legacy, 48_000, 144_000).unwrap();
        let finite_stem = synthesize_piecewise_crack_stem(&finite, 48_000, 144_000).unwrap();
        assert_eq!(finite_stem, legacy_stem);
    }

    #[test]
    fn finite_end_before_tangent_is_blast_only_with_endpoint_clock() {
        let segments = [BallisticMachSegment {
            length_m: 40.0,
            mach: 2.5,
        }];
        let plan = plan_piecewise_ballistic_shot(
            PiecewiseBallisticShot {
                muzzle_position_enu: EnuVector3::default(),
                direction_enu: EnuVector3::new(0.0, 1.0, 0.0),
                segments: &segments,
                levels: signed_shot().levels,
            },
            EnuVector3::new(0.0, 60.0, 30.0),
        )
        .unwrap();

        assert!(plan.tangent.is_none());
        assert!(plan.crack.is_none());
        assert_eq!(
            plan.trajectory_end.position_enu,
            EnuVector3::new(0.0, 40.0, 0.0)
        );
        assert_eq!(plan.trajectory_end.distance_from_muzzle_m, 40.0);
        assert_eq!(
            plan.trajectory_end.projectile_time_s.to_bits(),
            (40.0 / (2.5 * fightbox_api::ballistics::SOUND_SPEED_MPS)).to_bits()
        );
        assert_eq!(
            plan.trajectory_end.acoustic_distance_m,
            f64::from(20.0_f32.hypot(30.0))
        );
        assert_eq!(
            plan.trajectory_end.arrival_time_s.to_bits(),
            (plan.trajectory_end.projectile_time_s
                + plan.trajectory_end.acoustic_distance_m
                    / fightbox_api::ballistics::SOUND_SPEED_MPS)
                .to_bits()
        );
        let legacy = plan_ballistic_shot(signed_shot(), EnuVector3::new(0.0, 60.0, 30.0)).unwrap();
        assert_eq!(plan.blast, legacy.blast);
        assert_eq!(
            synthesize_piecewise_crack_stem(&plan, 48_000, 2_048).unwrap(),
            (vec![0.0; 2_048], -120.0)
        );
    }

    #[test]
    fn piecewise_world_plan_uses_second_leg_clock_and_endpoint() {
        let segments = [
            BallisticMachSegment {
                length_m: 40.0,
                mach: 3.0,
            },
            BallisticMachSegment {
                length_m: 80.0,
                mach: 2.0,
            },
        ];
        let plan = plan_piecewise_ballistic_shot(
            PiecewiseBallisticShot {
                muzzle_position_enu: EnuVector3::new(10.0, 20.0, 5.0),
                direction_enu: EnuVector3::new(0.0, 2.0, 0.0),
                segments: &segments,
                levels: signed_shot().levels,
            },
            EnuVector3::new(10.0, 90.0, 15.0),
        )
        .unwrap();
        let tangent = plan.tangent.unwrap();
        let local = solve_mach_cone_tangent(2.0, 10.0, 30.0);
        let first_leg_time_s = 40.0 / (3.0 * fightbox_api::ballistics::SOUND_SPEED_MPS);

        assert_eq!(tangent.segment_index, 1);
        assert_eq!(
            tangent.emission_time_s.to_bits(),
            (first_leg_time_s + local.t_star_s).to_bits()
        );
        assert_eq!(
            tangent.arrival_time_s.to_bits(),
            (first_leg_time_s + local.crack_arrival_time_s).to_bits()
        );
        assert_eq!(
            plan.trajectory_end.position_enu,
            EnuVector3::new(10.0, 140.0, 5.0)
        );
        assert_eq!(plan.trajectory_end.distance_from_muzzle_m, 120.0);
        assert!(plan.crack.unwrap().arrival_time_s < plan.blast.arrival_time_s);
    }

    #[test]
    fn subsonic_and_behind_muzzle_piecewise_controls_emit_no_crack() {
        let subsonic = [BallisticMachSegment {
            length_m: 100.0,
            mach: 0.9,
        }];
        let subsonic_plan = plan_piecewise_ballistic_shot(
            PiecewiseBallisticShot {
                muzzle_position_enu: EnuVector3::default(),
                direction_enu: EnuVector3::new(0.0, 1.0, 0.0),
                segments: &subsonic,
                levels: signed_shot().levels,
            },
            EnuVector3::new(0.0, 60.0, 30.0),
        )
        .unwrap();
        assert!(subsonic_plan.tangent.is_none());
        assert!(subsonic_plan.crack.is_none());
        assert!(subsonic_plan.blast.arrival_time_s.is_finite());
        assert!(subsonic_plan.trajectory_end.arrival_time_s.is_finite());

        let supersonic = [BallisticMachSegment {
            length_m: 100.0,
            mach: 2.5,
        }];
        let behind_plan = plan_piecewise_ballistic_shot(
            PiecewiseBallisticShot {
                muzzle_position_enu: EnuVector3::default(),
                direction_enu: EnuVector3::new(0.0, 1.0, 0.0),
                segments: &supersonic,
                levels: signed_shot().levels,
            },
            EnuVector3::new(0.0, -30.0, 30.0),
        )
        .unwrap();
        assert!(behind_plan.tangent.is_none());
        assert!(behind_plan.crack.is_none());
    }

    #[test]
    fn listener_at_trajectory_endpoint_has_finite_directionless_end_metadata() {
        let segments = [BallisticMachSegment {
            length_m: 100.0,
            mach: 2.0,
        }];
        let plan = plan_piecewise_ballistic_shot(
            PiecewiseBallisticShot {
                muzzle_position_enu: EnuVector3::default(),
                direction_enu: EnuVector3::new(0.0, 1.0, 0.0),
                segments: &segments,
                levels: signed_shot().levels,
            },
            EnuVector3::new(0.0, 100.0, 0.0),
        )
        .unwrap();

        assert!(plan.crack.is_none(), "the zero-miss axis is blast-only");
        assert_eq!(plan.trajectory_end.acoustic_distance_m, 0.0);
        assert_eq!(plan.trajectory_end.engine_propagation_delay_s, 0.0);
        assert!(plan.trajectory_end.arrival_direction_enu.is_none());
        assert!(plan.trajectory_end.arrival_time_s.is_finite());
    }

    #[test]
    fn non_supersonic_and_listener_at_muzzle_are_rejected_without_nonfinite_output() {
        let mut shot = signed_shot();
        shot.mach = 1.0;
        assert_eq!(
            plan_ballistic_shot(shot, EnuVector3::new(0.0, 60.0, 30.0)),
            Err(BallisticEventError::NonSupersonicMach)
        );
        assert_eq!(
            plan_ballistic_shot(signed_shot(), EnuVector3::default()),
            Err(BallisticEventError::DegenerateDirection)
        );
    }

    fn east_to_west_artillery_segments() -> [BallisticMachSegment; 1] {
        [BallisticMachSegment {
            length_m: 3_000.0,
            mach: 1.5,
        }]
    }

    fn east_to_west_artillery_shot(
        segments: &[BallisticMachSegment],
    ) -> PiecewiseBallisticShot<'_> {
        PiecewiseBallisticShot {
            muzzle_position_enu: EnuVector3::new(2_223.82, 102.5, 2_122.82),
            direction_enu: EnuVector3::new(-1.0, 0.0, -1.0),
            segments,
            levels: signed_shot().levels,
        }
    }

    #[test]
    fn default_overrides_reproduce_the_signed_planner_bit_for_bit() {
        let segments = east_to_west_artillery_segments();
        for listener in [
            EnuVector3::new(0.0, 60.0, 30.0),
            EnuVector3::new(434.02, 483.82, 1.5),
            EnuVector3::new(-300.0, 102.5, 1.5),
        ] {
            let shot = east_to_west_artillery_shot(&segments);
            assert_eq!(
                plan_piecewise_ballistic_shot_with(
                    shot,
                    listener,
                    BallisticPlanOverrides::default()
                ),
                plan_piecewise_ballistic_shot(shot, listener),
            );
        }
    }

    #[test]
    fn projectile_overrides_change_only_duration_and_crack_level() {
        let segments = east_to_west_artillery_segments();
        let shot = east_to_west_artillery_shot(&segments);
        let listener = EnuVector3::new(434.02, 483.82, 1.5);
        let signed = plan_piecewise_ballistic_shot(shot, listener).unwrap();
        let overrides = BallisticPlanOverrides {
            n_wave_reference_duration_ms: Some(2.8),
            crack_peak_db_at_reference: Some(150.8),
        };
        let scaled = plan_piecewise_ballistic_shot_with(shot, listener, overrides).unwrap();

        assert_eq!(scaled.tangent, signed.tangent);
        assert_eq!(scaled.trajectory_end, signed.trajectory_end);
        assert_eq!(
            scaled.whitham_level_offset_db,
            signed.whitham_level_offset_db
        );
        let (signed_crack, scaled_crack) = (signed.crack.unwrap(), scaled.crack.unwrap());
        assert_eq!(scaled_crack.position_enu, signed_crack.position_enu);
        assert_eq!(scaled_crack.arrival_time_s, signed_crack.arrival_time_s);
        assert!(
            (scaled.n_wave_duration_ms / signed.n_wave_duration_ms - 2.8 / 0.8).abs() < 1.0e-12
        );

        // Received peak = anchor + Whitham(b), independent of the terminal
        // segment's start (the signed blast back-solve would depend on it).
        let r_star_m = scaled.tangent.unwrap().acoustic_distance_m;
        let received_peak_db = scaled_crack.spl_at_one_meter_db - 20.0 * r_star_m.log10()
            + fightbox_api::ballistics::n_wave_crest_factor_db();
        assert!((received_peak_db - (150.8 + scaled.whitham_level_offset_db)).abs() < 1.0e-9);
        assert!(
            (received_peak_db - 133.19).abs() < 0.01,
            "{received_peak_db}"
        );

        assert_eq!(
            plan_piecewise_ballistic_shot_with(
                shot,
                listener,
                BallisticPlanOverrides {
                    n_wave_reference_duration_ms: Some(0.0),
                    ..BallisticPlanOverrides::default()
                },
            ),
            Err(BallisticEventError::InvalidOverride)
        );
    }

    #[test]
    fn slice_synthesis_matches_the_fixed_stem_and_its_crest_factor() {
        let plan = plan_ballistic_shot(signed_shot(), EnuVector3::new(0.0, 60.0, 30.0)).unwrap();
        let (stem, rms_dbfs) = synthesize_crack_stem(&plan, 48_000, 144_000).unwrap();
        let mut reused = vec![7.0; 144_000];
        let leading = (plan.tangent.t_star_s * 48_000.0).round() as usize;
        let reused_rms =
            synthesize_n_wave_into(plan.n_wave_duration_ms, 48_000, leading, &mut reused).unwrap();
        assert_eq!(reused, stem);
        assert_eq!(reused_rms.to_bits(), rms_dbfs.to_bits());

        // The sampled artillery-scale wave keeps the idealized crest factor
        // the peak anchor is converted with.
        let mut artillery = [0.0_f32; 512];
        let rms = synthesize_n_wave_into(5.5, 48_000, 0, &mut artillery).unwrap();
        assert_eq!(n_wave_frames(5.5, 48_000), 264);
        assert_eq!(artillery[0], 1.0);
        assert!(
            (f64::from(-rms) - fightbox_api::ballistics::n_wave_crest_factor_db()).abs() < 0.05
        );
        assert_eq!(
            synthesize_n_wave_into(5.5, 48_000, 300, &mut artillery),
            Err(BallisticEventError::InvalidProgramLength)
        );
    }
}
