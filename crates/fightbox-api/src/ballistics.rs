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

/// One finite constant-Mach leg of a straight projectile trajectory.
///
/// Positive subsonic Mach values are valid trajectory legs, but cannot emit a
/// crack candidate. This lets a finite plan retain its endpoint clock after a
/// projectile decelerates through Mach 1 without extending the shock path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticMachSegment {
    pub length_m: f64,
    pub mach: f64,
}

/// Why a finite piecewise trajectory admitted its earliest crack emission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiecewiseMachConeTangentKind {
    /// The ordinary stationary tangent lies inside one constant-Mach segment.
    SegmentInterior,
    /// A continuing supersonic deceleration makes the shared segment join the
    /// constrained minimum of the continuous arrival-time envelope.
    SegmentJoin,
}

/// Earliest crack candidate on a finite straight piecewise-Mach trajectory.
///
/// Distances are measured globally from the muzzle. `segment_index` identifies
/// the containing segment for an interior tangent and the segment ending at a
/// join. A join records the following Mach in `mach_after`; an interior tangent
/// leaves it `None`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PiecewiseMachConeTangent {
    pub kind: PiecewiseMachConeTangentKind,
    pub segment_index: usize,
    pub segment_start_m: f64,
    pub segment_end_m: f64,
    pub emission_distance_m: f64,
    pub emission_time_s: f64,
    pub acoustic_distance_m: f64,
    pub arrival_time_s: f64,
    pub mach_before: f64,
    pub mach_after: Option<f64>,
    /// Unit wave-travel direction in trajectory-local `[right, up, forward]`.
    pub crack_direction_listener: [f64; 3],
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

/// Finds the earliest valid crack on finite straight piecewise-Mach flight.
///
/// Each interior candidate uses [`solve_mach_cone_tangent`] in its segment's
/// local distance/time frame and is admitted only when its tangent lies inside
/// that segment. At an internal join, the projectile flight-time curve remains
/// continuous but its slope changes. If the arrival-time derivative is
/// non-positive on the left and non-negative on the right, the join itself is
/// the constrained envelope minimum; admitting it prevents a decelerating
/// piecewise approximation from opening a non-physical timing hole. The final
/// endpoint is deliberately not considered: when flight or supersonic emission
/// ends before the tangent, the result is `None`, leaving impact sound to its
/// separately authored event.
///
/// This pure helper allocates nothing. It returns `None` for invalid geometry
/// or segments as well as for a valid no-crack trajectory; the higher-level
/// planner validates input and distinguishes errors before calling it.
#[must_use]
pub fn solve_piecewise_mach_cone_tangent(
    segments: &[BallisticMachSegment],
    miss_distance_m: f64,
    s0_m: f64,
) -> Option<PiecewiseMachConeTangent> {
    if segments.is_empty()
        || !miss_distance_m.is_finite()
        || miss_distance_m <= 0.0
        || !s0_m.is_finite()
        || segments.iter().any(|segment| {
            !segment.length_m.is_finite()
                || segment.length_m <= 0.0
                || !segment.mach.is_finite()
                || segment.mach <= 0.0
        })
    {
        return None;
    }

    let mut best = None;
    let mut segment_start_m = 0.0_f64;
    let mut segment_start_time_s = 0.0_f64;
    for (segment_index, segment) in segments.iter().copied().enumerate() {
        let segment_end_m = segment_start_m + segment.length_m;
        if !segment_end_m.is_finite() || !segment_start_time_s.is_finite() {
            return None;
        }

        if segment.mach > 1.0 {
            let local =
                solve_mach_cone_tangent(segment.mach, miss_distance_m, s0_m - segment_start_m);
            if local.s_star_m >= 0.0
                && local.s_star_m <= segment.length_m
                && local.r_star_m.is_finite()
                && local.r_star_m > 0.0
            {
                choose_earliest_piecewise_candidate(
                    &mut best,
                    PiecewiseMachConeTangent {
                        kind: PiecewiseMachConeTangentKind::SegmentInterior,
                        segment_index,
                        segment_start_m,
                        segment_end_m,
                        emission_distance_m: segment_start_m + local.s_star_m,
                        emission_time_s: segment_start_time_s + local.t_star_s,
                        acoustic_distance_m: local.r_star_m,
                        arrival_time_s: segment_start_time_s + local.crack_arrival_time_s,
                        mach_before: segment.mach,
                        mach_after: None,
                        crack_direction_listener: local.crack_direction_listener,
                    },
                );
            }

            if let Some(next) = segments.get(segment_index + 1).copied()
                && next.mach > 1.0
            {
                let along_from_join_to_listener_m = s0_m - segment_end_m;
                let acoustic_distance_m = along_from_join_to_listener_m.hypot(miss_distance_m);
                if acoustic_distance_m.is_finite() && acoustic_distance_m > 0.0 {
                    let geometric_derivative = -along_from_join_to_listener_m / acoustic_distance_m;
                    let derivative_before = segment.mach.recip() + geometric_derivative;
                    let derivative_after = next.mach.recip() + geometric_derivative;
                    if derivative_before <= 0.0 && derivative_after >= 0.0 {
                        let join_time_s = segment_start_time_s
                            + segment.length_m / (segment.mach * SOUND_SPEED_MPS);
                        choose_earliest_piecewise_candidate(
                            &mut best,
                            PiecewiseMachConeTangent {
                                kind: PiecewiseMachConeTangentKind::SegmentJoin,
                                segment_index,
                                segment_start_m,
                                segment_end_m,
                                emission_distance_m: segment_end_m,
                                emission_time_s: join_time_s,
                                acoustic_distance_m,
                                arrival_time_s: join_time_s + acoustic_distance_m / SOUND_SPEED_MPS,
                                mach_before: segment.mach,
                                mach_after: Some(next.mach),
                                crack_direction_listener: [
                                    0.0,
                                    miss_distance_m / acoustic_distance_m,
                                    along_from_join_to_listener_m / acoustic_distance_m,
                                ],
                            },
                        );
                    }
                }
            }
        }

        segment_start_time_s += segment.length_m / (segment.mach * SOUND_SPEED_MPS);
        segment_start_m = segment_end_m;
    }
    best
}

fn choose_earliest_piecewise_candidate(
    best: &mut Option<PiecewiseMachConeTangent>,
    candidate: PiecewiseMachConeTangent,
) {
    if candidate.arrival_time_s.is_finite()
        && best.is_none_or(|current| candidate.arrival_time_s < current.arrival_time_s)
    {
        *best = Some(candidate);
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
    n_wave_duration_ms_from_reference(N_WAVE_REFERENCE_DURATION_MS, miss_distance_m)
}

/// Returns an N-wave duration for a projectile-scale reference, in milliseconds.
///
/// `reference_duration_ms` is the N-wave duration at
/// [`WHITHAM_REFERENCE_DISTANCE_M`]; it scales the same `d^(1/4)` law used by
/// [`n_wave_duration_ms`]. A larger projectile, such as an artillery shell,
/// declares a longer reference than the small-arms audition anchor. Both
/// arguments must be finite and positive.
#[must_use]
pub fn n_wave_duration_ms_from_reference(reference_duration_ms: f64, miss_distance_m: f64) -> f64 {
    reference_duration_ms
        * (miss_distance_m / WHITHAM_REFERENCE_DISTANCE_M).powf(N_WAVE_DURATION_DISTANCE_EXPONENT)
}

/// Peak-over-RMS ratio of the idealized auditioned N-wave, in decibels.
///
/// The two linear segments of the `45% / 55%` shape have mean square
/// `(0.45 * 1^2 + 0.55 * 0.818^2) / 3`, so the ratio is about `5.64 dB`. It
/// converts a peak-pressure level anchor to the renderer's RMS reference-level
/// convention. A sampled stem differs from this continuous value by only its
/// discretization error.
#[must_use]
pub fn n_wave_crest_factor_db() -> f64 {
    let mean_square = (N_WAVE_POSITIVE_FRACTION
        + N_WAVE_NEGATIVE_FRACTION * N_WAVE_NEGATIVE_PEAK * N_WAVE_NEGATIVE_PEAK)
        / 3.0;
    -10.0 * mean_square.log10()
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
    fn finite_single_segment_preserves_the_signed_tangent_and_rejects_after_impact() {
        let legacy = solve_mach_cone_tangent(MACH, 30.0, 60.0);
        let admitted = solve_piecewise_mach_cone_tangent(
            &[BallisticMachSegment {
                length_m: 50.0,
                mach: MACH,
            }],
            30.0,
            60.0,
        )
        .unwrap();
        assert_eq!(admitted.kind, PiecewiseMachConeTangentKind::SegmentInterior);
        assert_eq!(admitted.segment_index, 0);
        assert_eq!(
            admitted.emission_distance_m.to_bits(),
            legacy.s_star_m.to_bits()
        );
        assert_eq!(
            admitted.emission_time_s.to_bits(),
            legacy.t_star_s.to_bits()
        );
        assert_eq!(
            admitted.acoustic_distance_m.to_bits(),
            legacy.r_star_m.to_bits()
        );
        assert_eq!(
            admitted.arrival_time_s.to_bits(),
            legacy.crack_arrival_time_s.to_bits()
        );
        assert_eq!(
            admitted.crack_direction_listener,
            legacy.crack_direction_listener
        );

        assert!(
            solve_piecewise_mach_cone_tangent(
                &[BallisticMachSegment {
                    length_m: 40.0,
                    mach: MACH,
                }],
                30.0,
                60.0,
            )
            .is_none(),
            "a tangent beyond the impact endpoint became an impossible crack"
        );
    }

    #[test]
    fn piecewise_candidate_uses_cumulative_projectile_time() {
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
        let local = solve_mach_cone_tangent(2.0, 10.0, 30.0);
        let candidate = solve_piecewise_mach_cone_tangent(&segments, 10.0, 70.0).unwrap();
        let segment_start_time_s = 40.0 / (3.0 * SOUND_SPEED_MPS);

        assert_eq!(candidate.segment_index, 1);
        assert_eq!(
            candidate.kind,
            PiecewiseMachConeTangentKind::SegmentInterior
        );
        assert_eq!(
            candidate.emission_distance_m.to_bits(),
            (40.0 + local.s_star_m).to_bits()
        );
        assert_eq!(
            candidate.emission_time_s.to_bits(),
            (segment_start_time_s + local.t_star_s).to_bits()
        );
        assert_eq!(
            candidate.arrival_time_s.to_bits(),
            (segment_start_time_s + local.crack_arrival_time_s).to_bits()
        );
    }

    #[test]
    fn deceleration_join_closes_the_piecewise_timing_hole_continuously() {
        const MISS_M: f64 = 30.0;
        const JOIN_M: f64 = 100.0;
        const EPSILON_M: f64 = 1.0e-6;
        let segments = [
            BallisticMachSegment {
                length_m: JOIN_M,
                mach: 3.0,
            },
            BallisticMachSegment {
                length_m: 100.0,
                mach: 1.5,
            },
        ];
        let left_boundary_s0 = JOIN_M + MISS_M / (3.0_f64 * 3.0 - 1.0).sqrt();
        let right_boundary_s0 = JOIN_M + MISS_M / (1.5_f64 * 1.5 - 1.0).sqrt();

        let left_interior =
            solve_piecewise_mach_cone_tangent(&segments, MISS_M, left_boundary_s0 - EPSILON_M)
                .unwrap();
        let left_join =
            solve_piecewise_mach_cone_tangent(&segments, MISS_M, left_boundary_s0 + EPSILON_M)
                .unwrap();
        assert_eq!(
            left_interior.kind,
            PiecewiseMachConeTangentKind::SegmentInterior
        );
        assert_eq!(left_join.kind, PiecewiseMachConeTangentKind::SegmentJoin);
        assert!((left_join.arrival_time_s - left_interior.arrival_time_s).abs() < 1.0e-8);

        let right_join =
            solve_piecewise_mach_cone_tangent(&segments, MISS_M, right_boundary_s0 - EPSILON_M)
                .unwrap();
        let right_interior =
            solve_piecewise_mach_cone_tangent(&segments, MISS_M, right_boundary_s0 + EPSILON_M)
                .unwrap();
        assert_eq!(right_join.kind, PiecewiseMachConeTangentKind::SegmentJoin);
        assert_eq!(
            right_interior.kind,
            PiecewiseMachConeTangentKind::SegmentInterior
        );
        assert_eq!(right_interior.segment_index, 1);
        assert!((right_interior.arrival_time_s - right_join.arrival_time_s).abs() < 1.0e-8);
    }

    #[test]
    fn subsonic_finite_legs_never_emit_crack_candidates() {
        assert!(
            solve_piecewise_mach_cone_tangent(
                &[BallisticMachSegment {
                    length_m: 100.0,
                    mach: 0.9,
                }],
                30.0,
                60.0,
            )
            .is_none()
        );
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
    fn projectile_scale_reference_scales_the_default_duration_law() {
        for miss_distance_m in [0.5, 10.0, 30.0, 90.0, 357.25, 2_000.0] {
            assert_eq!(
                n_wave_duration_ms_from_reference(N_WAVE_REFERENCE_DURATION_MS, miss_distance_m)
                    .to_bits(),
                n_wave_duration_ms(miss_distance_m).to_bits(),
            );
        }
        assert_eq!(n_wave_duration_ms_from_reference(2.8, 30.0), 2.8);
        assert_printed(n_wave_duration_ms_from_reference(2.8, 480.0), 4, "5.6000");
    }

    #[test]
    fn idealized_n_wave_crest_factor_matches_its_segment_areas() {
        assert_printed(n_wave_crest_factor_db(), 4, "5.6427");
    }

    #[test]
    fn auditioned_n_wave_asymmetry_has_zero_idealized_area() {
        assert_printed(N_WAVE_NEGATIVE_PEAK, 6, "-0.818182");
        let signed_area =
            N_WAVE_POSITIVE_FRACTION + N_WAVE_NEGATIVE_FRACTION * N_WAVE_NEGATIVE_PEAK;
        assert!(signed_area.abs() <= f64::EPSILON);
    }
}
