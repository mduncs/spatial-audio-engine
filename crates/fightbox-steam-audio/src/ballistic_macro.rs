//! Adapter from finite ballistic physics to the retained macro event queue.
//!
//! The ballistic planner owns emission geometry and clocks. Macro transport
//! owns the already-computed remote/local path split. This module joins those
//! contracts without recomputing distance or atmospheric loss and presents the
//! resulting crack, blast, and optional impact as one atomic queue group.

use fightbox_api::ground::GroundAuthoringPolicy;
use fightbox_api::macro_transport::{EventRole, MacroEventId};
use fightbox_runtime::{
    EventAdmissionError, MacroEventScheduleRequest, MacroEventScheduler, MacroTransportPlan,
    ScheduledMacroEvent,
};

use crate::{
    BallisticEventSource, BallisticTrajectoryEnd, EchoProfile, PiecewiseBallisticShotPlan,
    SourceReflectionBudget,
};

/// Asset and lifetime fields copied verbatim into one dormant macro event.
///
/// `program_seek_frame` is the exact first frame to present when the physical
/// emission reaches local ingress. For a crack stem synthesized with embedded
/// pre-tangent silence, authoring points this at the N-wave onset; the adapter
/// separately schedules the tangent emission clock and never applies it twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BallisticMacroAssetBinding {
    pub event_id: MacroEventId,
    pub asset_key: u64,
    pub program_seek_frame: u64,
    pub retained_frames_after_activation: u64,
}

/// Renderer policy retained beside a queue event until its pinned role starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticMacroRenderPolicy {
    pub ground_authoring: GroundAuthoringPolicy,
    pub reflection_send_enabled: bool,
    pub reflection_budget: SourceReflectionBudget,
    pub echo_profile: EchoProfile,
}

impl BallisticMacroRenderPolicy {
    /// Mandatory V1 crack policy: no statistical ground, source reflections,
    /// or authored echo path. Direct HRTF, direct occlusion, and baked pathing
    /// remain separate renderer authorities.
    pub const CRACK_OFF: Self = Self {
        ground_authoring: GroundAuthoringPolicy::ForceOff,
        reflection_send_enabled: false,
        reflection_budget: SourceReflectionBudget::OFF,
        echo_profile: EchoProfile::OFF,
    };

    /// Ordinary source-specific transient treatment with no authored echo.
    pub const STANDARD: Self = Self {
        ground_authoring: GroundAuthoringPolicy::Default,
        reflection_send_enabled: true,
        reflection_budget: SourceReflectionBudget::STANDARD,
        echo_profile: EchoProfile::OFF,
    };

    /// Featured source-specific transient treatment with no authored echo.
    pub const CINEMATIC: Self = Self {
        ground_authoring: GroundAuthoringPolicy::Default,
        reflection_send_enabled: true,
        reflection_budget: SourceReflectionBudget::CINEMATIC,
        echo_profile: EchoProfile::OFF,
    };
}

impl Default for BallisticMacroRenderPolicy {
    fn default() -> Self {
        Self::STANDARD
    }
}

/// One already-planned macro ingress plus its asset and renderer authoring.
///
/// The transport must have been planned for the corresponding crack, muzzle,
/// or endpoint position. Its distance and atmosphere fields are copied to the
/// queue record exactly; the adapter does not evaluate either transfer again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticMacroIngressAuthoring {
    pub transport: MacroTransportPlan,
    pub asset: BallisticMacroAssetBinding,
    pub render_policy: BallisticMacroRenderPolicy,
}

/// Which shared impulse reservation an authored impact may occupy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BallisticImpactRole {
    Standard,
    Cinematic,
}

impl BallisticImpactRole {
    #[must_use]
    pub const fn event_role(self) -> EventRole {
        match self {
            Self::Standard => EventRole::StandardImpulse,
            Self::Cinematic => EventRole::CinematicImpulse,
        }
    }
}

/// Optional authored sound at the finite trajectory endpoint.
///
/// Ballistic physics supplies only the endpoint position and clock. The asset
/// key, calibrated program, tail lifetime, role, and render treatment remain
/// explicit authoring rather than being synthesized by the trajectory solver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticMacroImpactAuthoring {
    pub role: BallisticImpactRole,
    pub ingress: BallisticMacroIngressAuthoring,
}

/// Complete adapter request for one shot trigger.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticMacroGroupRequest {
    pub atomic_group_id: u64,
    pub trigger_frame: u64,
    pub sample_rate_hz: u32,
    /// Required exactly when the finite plan contains a valid crack.
    pub crack: Option<BallisticMacroIngressAuthoring>,
    pub blast: BallisticMacroIngressAuthoring,
    pub impact: Option<BallisticMacroImpactAuthoring>,
}

/// Ballistic physics retained next to one compact queue record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BallisticMacroPhysics {
    Crack(BallisticEventSource),
    Blast(BallisticEventSource),
    Impact(BallisticTrajectoryEnd),
}

/// One role-pinned member of an atomic ballistic macro group.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticMacroPlannedEvent {
    pub scheduled: ScheduledMacroEvent,
    pub physics: BallisticMacroPhysics,
    pub render_policy: BallisticMacroRenderPolicy,
}

/// Fixed-capacity, allocation-free crack/blast/impact admission group.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BallisticMacroGroup {
    events: [Option<BallisticMacroPlannedEvent>; EventRole::COUNT],
    count: u8,
}

impl BallisticMacroGroup {
    fn new() -> Self {
        Self {
            events: [None; EventRole::COUNT],
            count: 0,
        }
    }

    fn push(&mut self, event: BallisticMacroPlannedEvent) {
        let index = usize::from(self.count);
        debug_assert!(index < self.events.len());
        self.events[index] = Some(event);
        self.count += 1;
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn events(&self) -> impl ExactSizeIterator<Item = &BallisticMacroPlannedEvent> {
        self.events[..self.len()]
            .iter()
            .map(|event| event.as_ref().expect("the populated prefix is contiguous"))
    }

    #[must_use]
    pub fn event_for_role(&self, role: EventRole) -> Option<&BallisticMacroPlannedEvent> {
        self.events().find(|event| event.scheduled.role == role)
    }

    /// Atomically admits the complete populated prefix to the retained queue.
    /// No heap allocation occurs and a queue rejection cannot leave a partial
    /// crack/blast/impact family behind.
    pub fn admit(&self, scheduler: &mut MacroEventScheduler) -> Result<(), EventAdmissionError> {
        let mut scheduled = [ScheduledMacroEvent::default(); EventRole::COUNT];
        for (index, event) in self.events().enumerate() {
            scheduled[index] = event.scheduled;
        }
        scheduler.admit_group(&scheduled[..self.len()])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BallisticMacroAdapterError {
    InvalidSampleRate,
    MissingCrackIngress,
    UnexpectedCrackIngress,
    TransportEventIdentityMismatch(EventRole),
    TransportDelayMismatch(EventRole),
    InvalidTimeline(EventRole),
    Schedule {
        role: EventRole,
        source: EventAdmissionError,
    },
}

/// Maps one finite ballistic plan into a single atomic macro admission group.
///
/// Crack and endpoint flight offsets become physical emission clocks. The
/// supplied program seek is not adjusted, so asset authoring remains the only
/// authority for where playback begins. Supplied macro distance and atmosphere
/// values flow through [`MacroTransportPlan::schedule_event`] exactly once.
pub fn plan_ballistic_macro_group(
    plan: &PiecewiseBallisticShotPlan,
    request: BallisticMacroGroupRequest,
) -> Result<BallisticMacroGroup, BallisticMacroAdapterError> {
    if request.sample_rate_hz == 0 {
        return Err(BallisticMacroAdapterError::InvalidSampleRate);
    }
    match (plan.crack, request.crack) {
        (Some(_), None) => return Err(BallisticMacroAdapterError::MissingCrackIngress),
        (None, Some(_)) => return Err(BallisticMacroAdapterError::UnexpectedCrackIngress),
        _ => {}
    }

    let mut group = BallisticMacroGroup::new();
    if let (Some(source), Some(ingress)) = (plan.crack, request.crack) {
        group.push(schedule_member(
            request,
            ingress,
            EventRole::BallisticCrack,
            source.embedded_leading_silence_s,
            source.engine_propagation_delay_s,
            BallisticMacroPhysics::Crack(source),
            BallisticMacroRenderPolicy::CRACK_OFF,
        )?);
    }

    group.push(schedule_member(
        request,
        request.blast,
        EventRole::BallisticBlast,
        plan.blast.embedded_leading_silence_s,
        plan.blast.engine_propagation_delay_s,
        BallisticMacroPhysics::Blast(plan.blast),
        request.blast.render_policy,
    )?);

    if let Some(impact) = request.impact {
        group.push(schedule_member(
            request,
            impact.ingress,
            impact.role.event_role(),
            plan.trajectory_end.projectile_time_s,
            plan.trajectory_end.engine_propagation_delay_s,
            BallisticMacroPhysics::Impact(plan.trajectory_end),
            impact.ingress.render_policy,
        )?);
    }

    Ok(group)
}

fn schedule_member(
    request: BallisticMacroGroupRequest,
    ingress: BallisticMacroIngressAuthoring,
    role: EventRole,
    emission_offset_s: f64,
    expected_acoustic_delay_s: f64,
    physics: BallisticMacroPhysics,
    render_policy: BallisticMacroRenderPolicy,
) -> Result<BallisticMacroPlannedEvent, BallisticMacroAdapterError> {
    if ingress.transport.emitter_id != ingress.asset.event_id {
        return Err(BallisticMacroAdapterError::TransportEventIdentityMismatch(
            role,
        ));
    }
    let delay_error_s = (ingress.transport.total_delay_s - expected_acoustic_delay_s).abs();
    if !delay_error_s.is_finite() || delay_error_s > 1.0 / f64::from(request.sample_rate_hz) {
        return Err(BallisticMacroAdapterError::TransportDelayMismatch(role));
    }
    let offset_frames = seconds_to_frames(emission_offset_s, request.sample_rate_hz)
        .ok_or(BallisticMacroAdapterError::InvalidTimeline(role))?;
    let emission_frame = request
        .trigger_frame
        .checked_add(offset_frames)
        .ok_or(BallisticMacroAdapterError::InvalidTimeline(role))?;
    let scheduled = ingress
        .transport
        .schedule_event(MacroEventScheduleRequest {
            event_id: ingress.asset.event_id,
            atomic_group_id: request.atomic_group_id,
            role,
            asset_key: ingress.asset.asset_key,
            emission_frame,
            program_seek_frame: ingress.asset.program_seek_frame,
            retained_frames_after_activation: ingress.asset.retained_frames_after_activation,
            sample_rate_hz: request.sample_rate_hz,
        })
        .map_err(|source| BallisticMacroAdapterError::Schedule { role, source })?;

    Ok(BallisticMacroPlannedEvent {
        scheduled,
        physics,
        render_policy,
    })
}

fn seconds_to_frames(seconds: f64, sample_rate_hz: u32) -> Option<u64> {
    let frames = (seconds * f64::from(sample_rate_hz)).round();
    (frames.is_finite() && frames >= 0.0 && frames <= u64::MAX as f64).then_some(frames as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fightbox_api::EnuVector3;
    use fightbox_api::ballistics::BallisticMachSegment;
    use fightbox_api::macro_transport::{
        MacroAssetTransport, MacroEmitter, MacroListener, MacroTransportConfig,
    };
    use fightbox_runtime::{FrozenAtmosphere, plan_macro_transport};

    use crate::{BallisticEventLevels, PiecewiseBallisticShot, plan_piecewise_ballistic_shot};

    const SAMPLE_RATE: u32 = 48_000;
    const TRIGGER_FRAME: u64 = 96_000;

    fn shot_plan(length_m: f64) -> PiecewiseBallisticShotPlan {
        let segments = [BallisticMachSegment {
            length_m,
            mach: 2.5,
        }];
        plan_piecewise_ballistic_shot(
            PiecewiseBallisticShot {
                muzzle_position_enu: EnuVector3::default(),
                direction_enu: EnuVector3::new(0.0, 1.0, 0.0),
                segments: &segments,
                levels: BallisticEventLevels {
                    blast_spl_at_one_meter_db: 155.0,
                    crack_over_blast_db_at_reference: 3.0,
                },
            },
            EnuVector3::new(0.0, 60.0, 30.0),
        )
        .unwrap()
    }

    fn ingress(
        event_id: u64,
        asset_key: u64,
        seek_frame: u64,
        position: EnuVector3,
        policy: BallisticMacroRenderPolicy,
    ) -> BallisticMacroIngressAuthoring {
        let atmosphere = FrozenAtmosphere::freeze(None);
        let transport = plan_macro_transport(
            MacroEmitter {
                id: MacroEventId(event_id),
                position_enu: position,
                program_started_at_s: 0.0,
                asset_transport: MacroAssetTransport::Seekable,
                recording_carries_motion: false,
            },
            MacroListener {
                position_enu: EnuVector3::new(0.0, 60.0, 30.0),
                session_time_s: 0.0,
            },
            MacroTransportConfig {
                local_horizon_m: 20.0,
            },
            &atmosphere,
        )
        .unwrap();
        BallisticMacroIngressAuthoring {
            transport,
            asset: BallisticMacroAssetBinding {
                event_id: MacroEventId(event_id),
                asset_key,
                program_seek_frame: seek_frame,
                retained_frames_after_activation: 144_000,
            },
            render_policy: policy,
        }
    }

    fn reconstructed_arrival_frame(event: &BallisticMacroPlannedEvent) -> u64 {
        let acoustic_delay_s = match event.physics {
            BallisticMacroPhysics::Crack(source) | BallisticMacroPhysics::Blast(source) => {
                source.engine_propagation_delay_s
            }
            BallisticMacroPhysics::Impact(end) => end.engine_propagation_delay_s,
        };
        let macro_delay_frames = event
            .scheduled
            .ingress_activation_frame
            .saturating_sub(event.scheduled.emission_frame);
        let macro_delay_s = macro_delay_frames as f64 / f64::from(SAMPLE_RATE);
        let local_delay_frames =
            ((acoustic_delay_s - macro_delay_s) * f64::from(SAMPLE_RATE)).round() as u64;
        event.scheduled.ingress_activation_frame + local_delay_frames
    }

    #[test]
    fn worked_example_maps_to_one_atomic_crack_blast_group_without_reapplying_transfer() {
        let plan = shot_plan(500.0);
        let crack_source = plan.crack.unwrap();
        let enabled_echo = EchoProfile::from_loop_frames(
            48_000,
            &[0],
            fightbox_api::ImpulseClass::ArtilleryThunder,
        )
        .unwrap();
        let authored_crack_policy = BallisticMacroRenderPolicy {
            ground_authoring: GroundAuthoringPolicy::ForceOnNonNormative,
            reflection_send_enabled: true,
            reflection_budget: SourceReflectionBudget::CINEMATIC,
            echo_profile: enabled_echo,
        };
        let crack_seek =
            (crack_source.embedded_leading_silence_s * f64::from(SAMPLE_RATE)).round() as u64;
        let crack_ingress = ingress(
            101,
            10_001,
            crack_seek,
            crack_source.position_enu,
            authored_crack_policy,
        );
        let blast_ingress = ingress(
            102,
            10_002,
            17,
            plan.blast.position_enu,
            BallisticMacroRenderPolicy::CINEMATIC,
        );
        let crack_transfer = (
            crack_ingress.transport.macro_segment.distance_gain,
            crack_ingress.transport.macro_segment.atmosphere_gain_db,
        );
        let group = plan_ballistic_macro_group(
            &plan,
            BallisticMacroGroupRequest {
                atomic_group_id: 77,
                trigger_frame: TRIGGER_FRAME,
                sample_rate_hz: SAMPLE_RATE,
                crack: Some(crack_ingress),
                blast: blast_ingress,
                impact: None,
            },
        )
        .unwrap();

        assert_eq!(group.len(), 2);
        let crack = group.event_for_role(EventRole::BallisticCrack).unwrap();
        let blast = group.event_for_role(EventRole::BallisticBlast).unwrap();
        assert_eq!(crack.scheduled.atomic_group_id, 77);
        assert_eq!(blast.scheduled.atomic_group_id, 77);
        assert_eq!(crack.scheduled.asset_key, 10_001);
        assert_eq!(blast.scheduled.asset_key, 10_002);
        assert_eq!(crack.scheduled.program_seek_frame, crack_seek);
        assert_eq!(blast.scheduled.program_seek_frame, 17);
        assert_eq!(crack.scheduled.emission_frame, TRIGGER_FRAME + crack_seek);
        assert_eq!(blast.scheduled.emission_frame, TRIGGER_FRAME);
        assert_eq!(
            crack.scheduled.macro_distance_gain.to_bits(),
            crack_transfer.0.to_bits()
        );
        assert_eq!(crack.scheduled.macro_atmosphere_gain_db, crack_transfer.1);
        assert_eq!(
            crack.render_policy,
            BallisticMacroRenderPolicy::CRACK_OFF,
            "authored crack sends escaped the mandatory direct-only policy"
        );
        assert!(!crack.render_policy.echo_profile.is_enabled());
        assert_eq!(blast.render_policy, BallisticMacroRenderPolicy::CINEMATIC);

        let crack_expected =
            TRIGGER_FRAME + (crack_source.arrival_time_s * f64::from(SAMPLE_RATE)).round() as u64;
        let blast_expected =
            TRIGGER_FRAME + (plan.blast.arrival_time_s * f64::from(SAMPLE_RATE)).round() as u64;
        assert!(reconstructed_arrival_frame(crack).abs_diff(crack_expected) <= 1);
        assert!(reconstructed_arrival_frame(blast).abs_diff(blast_expected) <= 1);

        let mut scheduler = MacroEventScheduler::new();
        group.admit(&mut scheduler).unwrap();
        assert_eq!(scheduler.queued_len(), 2);
    }

    #[test]
    fn finite_no_crack_is_blast_only_until_an_authored_impact_is_present() {
        let plan = shot_plan(40.0);
        assert!(plan.crack.is_none());
        let blast = ingress(
            201,
            20_001,
            0,
            plan.blast.position_enu,
            BallisticMacroRenderPolicy::STANDARD,
        );
        let blast_only = plan_ballistic_macro_group(
            &plan,
            BallisticMacroGroupRequest {
                atomic_group_id: 88,
                trigger_frame: TRIGGER_FRAME,
                sample_rate_hz: SAMPLE_RATE,
                crack: None,
                blast,
                impact: None,
            },
        )
        .unwrap();
        assert_eq!(blast_only.len(), 1);
        assert!(
            blast_only
                .event_for_role(EventRole::BallisticCrack)
                .is_none()
        );
        assert!(
            blast_only
                .event_for_role(EventRole::BallisticBlast)
                .is_some()
        );

        for (index, role) in [
            BallisticImpactRole::Standard,
            BallisticImpactRole::Cinematic,
        ]
        .into_iter()
        .enumerate()
        {
            let impact = ingress(
                202 + index as u64,
                20_002 + index as u64,
                31 + index as u64,
                plan.trajectory_end.position_enu,
                if role == BallisticImpactRole::Standard {
                    BallisticMacroRenderPolicy::STANDARD
                } else {
                    BallisticMacroRenderPolicy::CINEMATIC
                },
            );
            let group = plan_ballistic_macro_group(
                &plan,
                BallisticMacroGroupRequest {
                    atomic_group_id: 90 + index as u64,
                    trigger_frame: TRIGGER_FRAME,
                    sample_rate_hz: SAMPLE_RATE,
                    crack: None,
                    blast,
                    impact: Some(BallisticMacroImpactAuthoring {
                        role,
                        ingress: impact,
                    }),
                },
            )
            .unwrap();
            let impact = group.event_for_role(role.event_role()).unwrap();
            let impact_offset_frames =
                (plan.trajectory_end.projectile_time_s * f64::from(SAMPLE_RATE)).round() as u64;
            assert_eq!(group.len(), 2);
            assert_eq!(
                impact.scheduled.emission_frame,
                TRIGGER_FRAME + impact_offset_frames
            );
            assert_eq!(impact.scheduled.program_seek_frame, 31 + index as u64);
            assert!(group.event_for_role(EventRole::BallisticCrack).is_none());
            let expected_arrival = TRIGGER_FRAME
                + (plan.trajectory_end.arrival_time_s * f64::from(SAMPLE_RATE)).round() as u64;
            assert!(reconstructed_arrival_frame(impact).abs_diff(expected_arrival) <= 1);

            let mut scheduler = MacroEventScheduler::new();
            group.admit(&mut scheduler).unwrap();
            assert_eq!(scheduler.queued_len(), 2);
        }
    }
}
