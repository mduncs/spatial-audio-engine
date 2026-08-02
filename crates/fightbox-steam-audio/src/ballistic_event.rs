//! Trigger-time planning for pre-declared supersonic shot source pairs.
//!
//! This sidecar owns no clock and creates no backend source. A host constructs
//! the crack and blast slots with its retained world, evaluates one plan at a
//! trigger boundary, teleports both slots in one `SimulationUpdate`, and puts
//! the returned leading silence into their dry stems. The renderer's existing
//! source-distance delay then completes each ballistic arrival time.

use fightbox_api::ballistics::{
    MachConeTangent, crack_spl_at_one_meter_db, listener_receives_crack, n_wave_duration_ms,
    solve_mach_cone_tangent, whitham_level_offset_db,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BallisticEventError {
    NonFiniteInput,
    DegenerateDirection,
    NonSupersonicMach,
    InvalidSampleRate,
    InvalidProgramLength,
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
    if sample_rate_hz == 0 {
        return Err(BallisticEventError::InvalidSampleRate);
    }
    let Some(crack) = plan.crack else {
        return Ok((vec![0.0; program_frames], -120.0));
    };
    let leading_frames = seconds_to_frames(crack.embedded_leading_silence_s, sample_rate_hz);
    let wave_frames = ((plan.n_wave_duration_ms / 1_000.0) * f64::from(sample_rate_hz))
        .round()
        .max(4.0) as usize;
    if leading_frames.saturating_add(wave_frames) > program_frames {
        return Err(BallisticEventError::InvalidProgramLength);
    }
    let mut stem = vec![0.0; program_frames];
    let wave = &mut stem[leading_frames..leading_frames + wave_frames];
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
    Ok((stem, (10.0 * mean_square.log10()) as f32))
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
}
