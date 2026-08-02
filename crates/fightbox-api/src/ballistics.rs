//! Pure Wave 12 ballistics arithmetic.
//!
//! This module deliberately contains no renderer or SDK types. It reproduces
//! the cone timing, apparent-direction, Whitham level, and N-wave duration
//! calculations used by the signed Wave 12 A-strip.

/// Speed of sound used by the signed Wave 12 strip, in metres per second.
///
/// This is a model anchor rather than an atmosphere-dependent calculation.
pub const SOUND_SPEED_MPS: f64 = 343.0;

/// Miss distance at which the Whitham level offset is `0 dB`, in metres.
pub const WHITHAM_REFERENCE_DISTANCE_M: f64 = 30.0;

/// Distance exponent for Whitham N-wave peak pressure.
///
/// Peak pressure follows `d^(-3/4)`; converting that amplitude ratio to
/// decibels gives the `-15 log10(d / d_ref)` law used by the signed strip.
pub const WHITHAM_LEVEL_DISTANCE_EXPONENT: f64 = -0.75;

/// N-wave duration at [`WHITHAM_REFERENCE_DISTANCE_M`], in milliseconds.
///
/// This `0.800 ms` anchor was accepted in the Wave 12 audition. It is an
/// audition parameter, not a universal constant of external ballistics.
pub const N_WAVE_REFERENCE_DURATION_MS: f64 = 0.800;

/// Distance exponent for the signed strip's N-wave duration law.
pub const N_WAVE_DURATION_DISTANCE_EXPONENT: f64 = 0.25;

/// Fraction of the auditioned N-wave occupied by its positive segment.
///
/// The `45% / 55%` asymmetry was accepted as an audition parameter. It should
/// not be treated as a universal Whitham waveform constant.
pub const N_WAVE_POSITIVE_FRACTION: f64 = 0.45;

/// Fraction of the auditioned N-wave occupied by its negative segment.
///
/// The `45% / 55%` asymmetry was accepted as an audition parameter. It should
/// not be treated as a universal Whitham waveform constant.
pub const N_WAVE_NEGATIVE_FRACTION: f64 = 0.55;

/// Signed peak of the auditioned N-wave's negative segment.
///
/// This is `-0.45 / 0.55 = -0.818182...`, which makes the two idealized
/// triangular segment areas cancel. The value belongs to the signed audition
/// shape and is not a universal Whitham waveform constant.
pub const N_WAVE_NEGATIVE_PEAK: f64 = -N_WAVE_POSITIVE_FRACTION / N_WAVE_NEGATIVE_FRACTION;

/// Closed-form tangent candidate for a straight, constant-Mach trajectory.
///
/// A negative [`s_star_m`](Self::s_star_m) is still a useful candidate result:
/// it identifies the prototype's no-crack zone. Use [`listener_receives_crack`]
/// to apply that rejection rule.
///
/// Direction arrays use the signed strip's listener-local audition frame in
/// `[right, up, forward]` order. The strip places the trajectory-axis component
/// on `+forward` and the non-negative miss component on `+up`, so both cues lie
/// in its vertical-ahead plane. These are apparent source/HRTF directions, not
/// world-space ENU positions. The renderer maps this local frame to Steam Audio
/// as `(x, y, z) = (right, up, -forward)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MachConeTangent {
    /// Along-track distance from the muzzle to the tangent emission point, `s*`.
    pub s_star_m: f64,
    /// Bullet flight time from the muzzle to the tangent emission point, `t*`.
    pub t_star_s: f64,
    /// Acoustic distance from the tangent emission point to the listener, `r*`.
    pub r_star_m: f64,
    /// Crack arrival time measured from the muzzle event.
    pub crack_arrival_time_s: f64,
    /// Straight-line acoustic distance from the muzzle to the listener.
    pub blast_distance_m: f64,
    /// Muzzle-blast arrival time measured from the muzzle event.
    pub blast_arrival_time_s: f64,
    /// Time by which the crack precedes the blast: `T_blast - T_crack`.
    pub lead_time_s: f64,
    /// Unit apparent crack direction in listener-local `[right, up, forward]` order.
    pub crack_direction_listener: [f64; 3],
    /// Unit apparent blast direction in listener-local `[right, up, forward]` order.
    pub blast_direction_listener: [f64; 3],
}

/// Solves the signed strip's straight-trajectory Mach-cone tangent geometry.
///
/// `mach` is `M > 1`, `miss_distance_m` is the non-negative perpendicular miss
/// distance `d`, and `s0_m` is the signed along-track distance from the muzzle
/// to the closest-approach reference. Positive `s0_m` lies in the trajectory's
/// direction. The calculation is the closed form in Part A, section A1 of
/// `docs/decisions/wave12-impulse-events.md`, with the prototype's fixed
/// [`SOUND_SPEED_MPS`].
///
/// The result includes a tangent candidate even when `s* < 0`; that candidate
/// is rejected separately by [`listener_receives_crack`]. Inputs outside the
/// documented finite, supersonic domain follow ordinary `f64` propagation and
/// can produce non-finite fields.
#[must_use]
pub fn solve_mach_cone_tangent(mach: f64, miss_distance_m: f64, s0_m: f64) -> MachConeTangent {
    let mach_root = (mach * mach - 1.0).sqrt();
    let bullet_speed_mps = mach * SOUND_SPEED_MPS;
    let s_star_m = s0_m - miss_distance_m / mach_root;
    let t_star_s = s_star_m / bullet_speed_mps;
    let r_star_m = miss_distance_m * mach / mach_root;
    let crack_arrival_time_s = t_star_s + r_star_m / SOUND_SPEED_MPS;
    let blast_distance_m = s0_m.hypot(miss_distance_m);
    let blast_arrival_time_s = blast_distance_m / SOUND_SPEED_MPS;
    let crack_forward = 1.0 / mach;
    let crack_up = mach_root / mach;
    let blast_forward = s0_m / blast_distance_m;
    let blast_up = miss_distance_m / blast_distance_m;

    MachConeTangent {
        s_star_m,
        t_star_s,
        r_star_m,
        crack_arrival_time_s,
        blast_distance_m,
        blast_arrival_time_s,
        lead_time_s: blast_arrival_time_s - crack_arrival_time_s,
        crack_direction_listener: [0.0, crack_up, crack_forward],
        blast_direction_listener: [0.0, blast_up, blast_forward],
    }
}

/// Returns whether the prototype accepts a crack candidate for this listener.
///
/// For finite inputs, this applies the signed prototype's exact rejection rule:
/// `M > 1` and `s* >= 0`. The straight trajectory is otherwise unbounded; this
/// predicate therefore has no impact-distance or end-of-supersonic-segment
/// parameter. Non-finite inputs and negative miss distances are rejected.
#[must_use]
pub fn listener_receives_crack(mach: f64, miss_distance_m: f64, s0_m: f64) -> bool {
    if !mach.is_finite()
        || mach <= 1.0
        || !miss_distance_m.is_finite()
        || miss_distance_m < 0.0
        || !s0_m.is_finite()
    {
        return false;
    }

    let mach_root = (mach * mach - 1.0).sqrt();
    let s_star_m = s0_m - miss_distance_m / mach_root;
    s_star_m >= 0.0
}

/// Returns the Whitham crack-level offset at a miss distance, in decibels.
///
/// The peak-pressure law is `d^(-3/4)`, anchored to `0 dB` at
/// [`WHITHAM_REFERENCE_DISTANCE_M`]. This is exactly the signed prototype's
/// `-15 log10(d / 30 m)` arithmetic. `miss_distance_m` must be finite and
/// positive.
#[must_use]
pub fn whitham_level_offset_db(miss_distance_m: f64) -> f64 {
    20.0 * WHITHAM_LEVEL_DISTANCE_EXPONENT
        * (miss_distance_m / WHITHAM_REFERENCE_DISTANCE_M).log10()
}

/// Returns the signed strip's N-wave duration at a miss distance, in milliseconds.
///
/// Duration follows `d^(1/4)`, anchored to
/// [`N_WAVE_REFERENCE_DURATION_MS`] at
/// [`WHITHAM_REFERENCE_DISTANCE_M`]. `miss_distance_m` must be finite and
/// positive.
#[must_use]
pub fn n_wave_duration_ms(miss_distance_m: f64) -> f64 {
    N_WAVE_REFERENCE_DURATION_MS
        * (miss_distance_m / WHITHAM_REFERENCE_DISTANCE_M).powf(N_WAVE_DURATION_DISTANCE_EXPONENT)
}

/// Back-solves the crack source's declared SPL at one metre.
///
/// `blast_reference_received_db` is the blast reference level at the listener,
/// matching the prototype's measured `reference_blast_peak` anchor.
/// `crack_over_blast_db` is the complete desired received offset, including any
/// class offset and [`whitham_level_offset_db`] contribution. `r_star_m` is the
/// static crack virtual source's tangent distance.
///
/// Part A, section A1 of `docs/decisions/wave12-impulse-events.md` requires the
/// sidecar to back-solve `SplAtOneMeter` so the existing inverse-distance gain
/// chain, applied exactly once, reaches the target. In decibels that inverse is
/// `blast_reference_received_db + crack_over_blast_db + 20 log10(r* / 1 m)`.
/// `r_star_m` must be finite and positive.
#[must_use]
pub fn crack_spl_at_one_meter_db(
    blast_reference_received_db: f64,
    crack_over_blast_db: f64,
    r_star_m: f64,
) -> f64 {
    blast_reference_received_db + crack_over_blast_db + 20.0 * r_star_m.log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACH: f64 = 2.5;

    fn assert_printed(value: f64, decimal_places: usize, expected: &str) {
        assert_eq!(format!("{value:.decimal_places$}"), expected);
    }

    fn assert_printed_signed(value: f64, decimal_places: usize, expected: &str) {
        assert_eq!(format!("{value:+.decimal_places$}"), expected);
    }

    fn direction_elevation_deg(direction: [f64; 3]) -> f64 {
        direction[1].atan2(direction[2]).to_degrees()
    }

    fn assert_unit(direction: [f64; 3]) {
        let length = direction
            .into_iter()
            .map(|value| value * value)
            .sum::<f64>()
            .sqrt();
        assert!((length - 1.0).abs() <= f64::EPSILON * 4.0);
    }

    #[test]
    fn worked_example_matches_signed_strip_at_printed_precision() {
        let solution = solve_mach_cone_tangent(MACH, 30.0, 60.0);

        assert_printed(solution.s_star_m, 4, "46.9069");
        assert_printed(solution.t_star_s * 1_000.0, 4, "54.7020");
        assert_printed(solution.r_star_m, 4, "32.7327");
        assert_printed(solution.crack_arrival_time_s * 1_000.0, 4, "150.1325");
        assert_printed(solution.blast_arrival_time_s * 1_000.0, 4, "195.5745");
        assert_printed(solution.lead_time_s * 1_000.0, 4, "45.4419");
        assert_printed(
            direction_elevation_deg(solution.crack_direction_listener),
            4,
            "66.4218",
        );
        assert_printed(
            direction_elevation_deg(solution.blast_direction_listener),
            4,
            "26.5651",
        );
        assert_unit(solution.crack_direction_listener);
        assert_unit(solution.blast_direction_listener);
        assert!(listener_receives_crack(MACH, 30.0, 60.0));
    }

    #[test]
    fn ten_metre_vector_matches_signed_strip_at_printed_precision() {
        let solution = solve_mach_cone_tangent(MACH, 10.0, 60.0);

        assert_printed(solution.lead_time_s * 1_000.0, 4, "80.6486");
        assert_printed_signed(whitham_level_offset_db(10.0), 4, "+7.1568");
        assert_printed(n_wave_duration_ms(10.0), 4, "0.6079");
    }

    #[test]
    fn ninety_metre_vector_matches_signed_strip_at_printed_precision() {
        let solution = solve_mach_cone_tangent(MACH, 90.0, 60.0);

        assert_printed(solution.lead_time_s * 1_000.0, 4, "4.8985");
        assert_printed_signed(whitham_level_offset_db(90.0), 4, "-7.1568");
        assert_printed(n_wave_duration_ms(90.0), 4, "1.0529");
    }

    #[test]
    fn negative_tangent_candidate_is_blast_only() {
        let solution = solve_mach_cone_tangent(MACH, 30.0, -30.0);

        assert_printed(solution.s_star_m, 4, "-43.0931");
        assert_printed(solution.blast_arrival_time_s * 1_000.0, 4, "123.6921");
        assert!(!listener_receives_crack(MACH, 30.0, -30.0));
    }

    #[test]
    fn subsonic_and_invalid_trajectories_are_rejected() {
        assert!(!listener_receives_crack(0.9, 30.0, 60.0));
        assert!(!listener_receives_crack(f64::NAN, 30.0, 60.0));
        assert!(!listener_receives_crack(MACH, -30.0, 60.0));
    }

    #[test]
    fn spl_back_solve_inverts_the_one_distance_gain() {
        let solution = solve_mach_cone_tangent(MACH, 30.0, 60.0);
        let declared_spl_db = crack_spl_at_one_meter_db(100.0, 3.0, solution.r_star_m);
        let received_spl_db = declared_spl_db - 20.0 * solution.r_star_m.log10();

        assert_printed(declared_spl_db, 4, "133.2996");
        assert!((received_spl_db - 103.0).abs() <= 1.0e-12);
    }

    #[test]
    fn auditioned_n_wave_asymmetry_has_zero_idealized_area() {
        assert_printed(N_WAVE_NEGATIVE_PEAK, 6, "-0.818182");
        let signed_area =
            N_WAVE_POSITIVE_FRACTION + N_WAVE_NEGATIVE_FRACTION * N_WAVE_NEGATIVE_PEAK;
        assert!(signed_area.abs() <= f64::EPSILON);
    }
}
