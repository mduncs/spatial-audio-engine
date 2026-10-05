//! Macro-event admission into one listener-local acoustic cell.
//!
//! [`MacroLocalIngress`] owns the dormant macro queue and consumes each due
//! event exactly once. Cell publication may change between events, but an
//! already-admitted tail pins the authority snapshot it entered with.

use std::sync::Arc;

use fightbox_api::EnuVector3;
use fightbox_api::macro_transport::{EventPropagationEligibility, EventRole, MacroEventId};
use fightbox_api::spectral::SPECTRAL_BAND_COUNT;

use crate::{
    CellIdentity, EventAdmissionError, EventEchoBindingError, EventReleaseError,
    EventReservationBatch, EventReservationError, FrozenAtmosphere, MACRO_SPEED_OF_SOUND_MPS,
    MacroEventScheduler, ScheduledMacroEvent,
};

/// One content-addressed artifact prepared for a specific cell generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellArtifactIdentity {
    pub cell: CellIdentity,
    pub world_generation: u64,
    pub content_sha256: String,
}

impl CellArtifactIdentity {
    #[must_use]
    pub fn new(
        cell: CellIdentity,
        world_generation: u64,
        content_sha256: impl Into<String>,
    ) -> Self {
        Self {
            cell,
            world_generation,
            content_sha256: content_sha256.into(),
        }
    }

    fn is_current_for(&self, cell: &CellIdentity, world_generation: u64) -> bool {
        self.cell == *cell
            && self.world_generation == world_generation
            && is_lowercase_sha256(&self.content_sha256)
    }
}

/// Cell-local identities that a detailed event can pin through its full tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCellAuthority {
    pub cell: CellIdentity,
    pub world_generation: u64,
    pub package: Option<CellArtifactIdentity>,
    pub probe_bake: Option<CellArtifactIdentity>,
    pub echo_authority: Option<CellArtifactIdentity>,
}

/// Why a due event used the audible macro fallback instead of detailed local
/// propagation. Every due event still produces an activation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressFallbackReason {
    MissingActiveCell,
    StaleActiveCell,
    MissingPackageAuthority,
    StalePackageAuthority,
    MissingProbeBakeAuthority,
    StaleProbeBakeAuthority,
    MissingEchoAuthority,
    StaleEchoAuthority,
    InvalidLocalLeg,
}

/// Linkage retained across the separately timed members of one event family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressEventFamily {
    General { atomic_group_id: u64 },
    FiniteBallistic { atomic_group_id: u64 },
}

impl IngressEventFamily {
    #[must_use]
    pub const fn atomic_group_id(self) -> u64 {
        match self {
            Self::General { atomic_group_id } | Self::FiniteBallistic { atomic_group_id } => {
                atomic_group_id
            }
        }
    }
}

/// The already-accounted macro segment. It is emitted only when its queue
/// record is consumed; the detailed backend receives none of these terms.
#[derive(Debug, PartialEq)]
pub struct MacroIngressConditioning {
    arrival_frame: u64,
    distance_gain: f32,
    atmosphere_gain_db: [f32; SPECTRAL_BAND_COUNT],
}

impl MacroIngressConditioning {
    #[must_use]
    pub const fn arrival_frame(&self) -> u64 {
        self.arrival_frame
    }

    #[must_use]
    pub const fn distance_gain(&self) -> f32 {
        self.distance_gain
    }

    #[must_use]
    pub const fn atmosphere_gain_db(&self) -> &[f32; SPECTRAL_BAND_COUNT] {
        &self.atmosphere_gain_db
    }
}

/// The only path section handed to detailed local propagation or, when local
/// authority is unavailable, to the macro fallback renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FinalLocalLeg {
    pub ingress_proxy_enu: EnuVector3,
    pub remote_direction_enu: EnuVector3,
    pub distance_m: f32,
    pub delay_frames: u64,
    pub distance_gain: f32,
    pub atmosphere_gain_db: [f32; SPECTRAL_BAND_COUNT],
}

/// Which renderer owns the final local leg.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngressRenderAuthority {
    DetailedLocal {
        authority: Arc<LocalCellAuthority>,
    },
    MacroFallback {
        observed_authority: Option<Arc<LocalCellAuthority>>,
        reason: IngressFallbackReason,
        shared_diffuse: bool,
    },
}

/// One queue event after macro arrival and cell-authority resolution.
#[derive(Debug, PartialEq)]
pub struct LocalIngressActivation {
    pub event_id: MacroEventId,
    pub family: IngressEventFamily,
    pub role: EventRole,
    pub asset_key: u64,
    pub emission_frame: u64,
    pub program_seek_frame: u64,
    pub tail_deadline_frame: u64,
    pub eligibility: EventPropagationEligibility,
    pub macro_conditioning: MacroIngressConditioning,
    pub local_leg: FinalLocalLeg,
    pub render_authority: IngressRenderAuthority,
}

impl LocalIngressActivation {
    /// Frame at which the fallback renderer reaches the ear after preserving
    /// the same final-leg delay the detailed backend would have owned.
    #[must_use]
    pub fn fallback_ear_arrival_frame(&self) -> Option<u64> {
        matches!(
            self.render_authority,
            IngressRenderAuthority::MacroFallback { .. }
        )
        .then(|| {
            self.macro_conditioning
                .arrival_frame
                .saturating_add(self.local_leg.delay_frames)
        })
    }
}

/// Fixed-capacity result of one due-event pass.
#[derive(Debug, PartialEq)]
pub struct LocalIngressActivationBatch {
    count: u8,
    activations: [Option<LocalIngressActivation>; EventRole::COUNT],
}

impl LocalIngressActivationBatch {
    fn new() -> Self {
        Self {
            count: 0,
            activations: std::array::from_fn(|_| None),
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &LocalIngressActivation> {
        self.activations[..self.len()]
            .iter()
            .filter_map(Option::as_ref)
    }

    /// Moves the consumed queue records into a backend adapter without cloning
    /// their exactly-once macro-conditioning tokens.
    pub fn into_activations(self) -> impl Iterator<Item = LocalIngressActivation> {
        self.activations
            .into_iter()
            .filter_map(|activation| activation)
    }

    #[must_use]
    pub fn for_role(&self, role: EventRole) -> Option<&LocalIngressActivation> {
        self.iter().find(|activation| activation.role == role)
    }

    pub fn extend_tail_deadline(
        &mut self,
        role: EventRole,
        event_id: MacroEventId,
        tail_deadline_frame: u64,
    ) -> bool {
        let len = self.len();
        let Some(activation) = self.activations[..len]
            .iter_mut()
            .filter_map(Option::as_mut)
            .find(|activation| activation.role == role && activation.event_id == event_id)
        else {
            return false;
        };
        if tail_deadline_frame < activation.tail_deadline_frame {
            return false;
        }
        activation.tail_deadline_frame = tail_deadline_frame;
        true
    }

    fn push(&mut self, activation: LocalIngressActivation) {
        let index = self.len();
        self.activations[index] = Some(activation);
        self.count += 1;
    }
}

/// Authority pinned by an already-admitted event until deadline or explicit
/// energy-floor release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedIngressTail {
    pub event_id: MacroEventId,
    pub family: IngressEventFamily,
    pub role: EventRole,
    pub tail_deadline_frame: u64,
    pub render_authority: IngressRenderAuthority,
}

/// Stable control-thread counters. Fallback is audible and always counted; it
/// is never represented as a dropped event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacroIngressTelemetry {
    pub selected_cell: CellIdentity,
    pub published_cell: Option<CellIdentity>,
    pub queued_event_count: usize,
    pub detailed_activation_count: u64,
    pub fallback_activation_count: u64,
    pub last_fallback_event: Option<MacroEventId>,
    pub last_fallback_reason: Option<IngressFallbackReason>,
    pub active_tail_events: [Option<MacroEventId>; EventRole::COUNT],
}

/// Input sampled at one macro-arrival pass.
#[derive(Clone, Copy)]
pub struct IngressArrivalContext<'a> {
    pub current_frame: u64,
    pub sample_rate_hz: u32,
    pub listener_position_enu: EnuVector3,
    pub atmosphere: &'a FrozenAtmosphere,
}

/// Owns the 1,024-event queue, the current cell publication, and the four
/// admitted role tails.
pub struct MacroLocalIngress {
    scheduler: MacroEventScheduler,
    selected_cell: CellIdentity,
    published_authority: Option<Arc<LocalCellAuthority>>,
    tails: [Option<AdmittedIngressTail>; EventRole::COUNT],
    detailed_activation_count: u64,
    fallback_activation_count: u64,
    last_fallback_event: Option<MacroEventId>,
    last_fallback_reason: Option<IngressFallbackReason>,
}

impl MacroLocalIngress {
    #[must_use]
    pub fn new(
        selected_cell: CellIdentity,
        published_authority: Option<LocalCellAuthority>,
    ) -> Self {
        Self {
            scheduler: MacroEventScheduler::new(),
            selected_cell,
            published_authority: published_authority.map(Arc::new),
            tails: std::array::from_fn(|_| None),
            detailed_activation_count: 0,
            fallback_activation_count: 0,
            last_fallback_event: None,
            last_fallback_reason: None,
        }
    }

    /// Moves new-event authority. Existing entries in `tails` retain their
    /// previous `Arc`, so a swap cannot steal or reinterpret an admitted tail.
    pub fn publish_active_cell(
        &mut self,
        selected_cell: CellIdentity,
        published_authority: Option<LocalCellAuthority>,
    ) {
        self.selected_cell = selected_cell;
        self.published_authority = published_authority.map(Arc::new);
    }

    pub fn admit_group(
        &mut self,
        events: &[ScheduledMacroEvent],
    ) -> Result<(), EventAdmissionError> {
        self.scheduler.admit_group(events)
    }

    pub fn bind_echo_anchor(
        &mut self,
        event_id: MacroEventId,
        anchor_key: [u8; 16],
    ) -> Result<(), EventEchoBindingError> {
        self.scheduler.bind_echo_anchor(event_id, anchor_key)
    }

    #[must_use]
    pub const fn queued_len(&self) -> usize {
        self.scheduler.queued_len()
    }

    /// Consumes each due queue record once, resolves the cell snapshot at that
    /// instant, and returns either detailed-local or audible macro fallback.
    pub fn activate_due(
        &mut self,
        context: IngressArrivalContext<'_>,
    ) -> LocalIngressActivationBatch {
        self.retire_due(context.current_frame);
        let due = self.scheduler.activate_due(context.current_frame);
        let mut output = LocalIngressActivationBatch::new();
        for event in due.events[..usize::from(due.count)].iter().copied() {
            let activation = self.preview_event(event, context);
            self.scheduler
                .extend_active_tail(event.role, event.event_id, activation.tail_deadline_frame)
                .expect("the scheduler just activated this exact ingress event");
            self.record_committed_activation(&activation);
            output.push(activation);
        }
        output
    }

    /// Holds the earliest common activation frame at or before `lookahead`
    /// dormant in the fixed scheduler. No voice, tail, authority, or telemetry
    /// mutation occurs before direct success.
    pub fn reserve_next_activation(&mut self, lookahead_frame: u64) -> EventReservationBatch {
        self.scheduler.reserve_next_activation(lookahead_frame)
    }

    /// Resolves the current cell and final local leg without consuming the
    /// dormant reservation. Any cloned authority remains control-owned in the
    /// returned preview and may be dropped safely after a direct failure.
    pub fn preview_reserved(
        &self,
        reservation: EventReservationBatch,
        context: IngressArrivalContext<'_>,
    ) -> Result<LocalIngressActivationBatch, EventReservationError> {
        self.scheduler.validate_reserved(reservation)?;
        if reservation.activation_frame() != Some(context.current_frame) {
            return Err(EventReservationError::FrameMismatch);
        }
        let mut output = LocalIngressActivationBatch::new();
        for event in reservation.iter().copied() {
            output.push(self.preview_event(event, context));
        }
        Ok(output)
    }

    /// Proves the scalar preview still names the exact held batch and that its
    /// role voices will be available at the transaction frame. The FFI bridge
    /// runs this before direct simulation, making the later serialized commit
    /// infallible unless an internal invariant is violated.
    pub fn validate_reserved_commit(
        &self,
        reservation: EventReservationBatch,
        preview: &LocalIngressActivationBatch,
        current_frame: u64,
    ) -> Result<(), EventReservationError> {
        if preview.len() != reservation.len()
            || reservation.iter().any(|event| {
                preview.for_role(event.role).is_none_or(|activation| {
                    activation.event_id != event.event_id
                        || activation.family.atomic_group_id() != event.atomic_group_id
                        || activation.asset_key != event.asset_key
                        || activation.emission_frame != event.emission_frame
                        || activation.program_seek_frame != event.program_seek_frame
                        || activation.macro_conditioning.arrival_frame()
                            != event.ingress_activation_frame
                })
            })
        {
            return Err(EventReservationError::IdentityMismatch);
        }
        self.scheduler
            .validate_reserved_commit(reservation, current_frame)
    }

    /// Commits a previously previewed reservation after the matching direct
    /// generation succeeded. Queue/voices mutate once; telemetry and pinned
    /// authority are installed only here.
    pub fn commit_reserved(
        &mut self,
        reservation: EventReservationBatch,
        preview: LocalIngressActivationBatch,
        current_frame: u64,
    ) -> Result<LocalIngressActivationBatch, EventReservationError> {
        self.validate_reserved_commit(reservation, &preview, current_frame)?;
        let committed = self.scheduler.commit_reserved(reservation, current_frame)?;
        for event in committed.events[..usize::from(committed.count)]
            .iter()
            .copied()
        {
            let activation = preview
                .for_role(event.role)
                .expect("the preview identity was validated before mutation");
            self.scheduler
                .extend_active_tail(event.role, event.event_id, activation.tail_deadline_frame)
                .expect("the scheduler just committed this exact ingress event");
            self.record_committed_activation(activation);
        }
        Ok(preview)
    }

    pub fn discard_reserved(
        &mut self,
        reservation: EventReservationBatch,
    ) -> Result<(), EventReservationError> {
        self.scheduler.discard_reserved(reservation)
    }

    #[must_use]
    pub fn release_matches(&self, role: EventRole, event_id: MacroEventId) -> bool {
        self.scheduler.active_event(role) == Some(event_id)
            && self.tails[role.index()]
                .as_ref()
                .is_some_and(|tail| tail.event_id == event_id)
    }

    pub fn release(
        &mut self,
        role: EventRole,
        event_id: MacroEventId,
    ) -> Result<(), EventReleaseError> {
        self.scheduler.release(role, event_id)?;
        if self.tails[role.index()]
            .as_ref()
            .is_some_and(|tail| tail.event_id == event_id)
        {
            self.tails[role.index()] = None;
        }
        Ok(())
    }

    #[must_use]
    pub fn admitted_tail(&self, role: EventRole) -> Option<&AdmittedIngressTail> {
        self.tails[role.index()].as_ref()
    }

    #[must_use]
    pub fn telemetry(&self) -> MacroIngressTelemetry {
        MacroIngressTelemetry {
            selected_cell: self.selected_cell.clone(),
            published_cell: self
                .published_authority
                .as_ref()
                .map(|authority| authority.cell.clone()),
            queued_event_count: self.scheduler.queued_len(),
            detailed_activation_count: self.detailed_activation_count,
            fallback_activation_count: self.fallback_activation_count,
            last_fallback_event: self.last_fallback_event,
            last_fallback_reason: self.last_fallback_reason,
            active_tail_events: std::array::from_fn(|index| {
                self.tails[index].as_ref().map(|tail| tail.event_id)
            }),
        }
    }

    fn retire_due(&mut self, current_frame: u64) {
        for tail in &mut self.tails {
            if tail
                .as_ref()
                .is_some_and(|tail| tail.tail_deadline_frame <= current_frame)
            {
                *tail = None;
            }
        }
    }

    fn preview_event(
        &self,
        event: ScheduledMacroEvent,
        context: IngressArrivalContext<'_>,
    ) -> LocalIngressActivation {
        let eligibility = event.role.local_propagation_eligibility();
        let (local_leg, local_leg_valid) = final_local_leg(event, context);
        let authority = if local_leg_valid {
            resolve_authority(
                &self.selected_cell,
                self.published_authority.as_ref(),
                eligibility,
            )
        } else {
            Err(IngressFallbackReason::InvalidLocalLeg)
        };
        let render_authority = match authority {
            Ok(authority) => IngressRenderAuthority::DetailedLocal { authority },
            Err(reason) => IngressRenderAuthority::MacroFallback {
                observed_authority: self.published_authority.clone(),
                reason,
                shared_diffuse: eligibility.shared_diffuse,
            },
        };
        LocalIngressActivation {
            event_id: event.event_id,
            family: event_family(event),
            role: event.role,
            asset_key: event.asset_key,
            emission_frame: event.emission_frame,
            program_seek_frame: event.program_seek_frame,
            tail_deadline_frame: event
                .tail_deadline_frame
                .saturating_add(local_leg.delay_frames),
            eligibility,
            macro_conditioning: MacroIngressConditioning {
                arrival_frame: event.ingress_activation_frame,
                distance_gain: event.macro_distance_gain,
                atmosphere_gain_db: event.macro_atmosphere_gain_db,
            },
            local_leg,
            render_authority,
        }
    }

    fn record_committed_activation(&mut self, activation: &LocalIngressActivation) {
        match &activation.render_authority {
            IngressRenderAuthority::DetailedLocal { .. } => {
                self.detailed_activation_count = self.detailed_activation_count.saturating_add(1);
            }
            IngressRenderAuthority::MacroFallback { reason, .. } => {
                self.fallback_activation_count = self.fallback_activation_count.saturating_add(1);
                self.last_fallback_event = Some(activation.event_id);
                self.last_fallback_reason = Some(*reason);
            }
        }
        self.tails[activation.role.index()] = Some(AdmittedIngressTail {
            event_id: activation.event_id,
            family: activation.family,
            role: activation.role,
            tail_deadline_frame: activation.tail_deadline_frame,
            render_authority: activation.render_authority.clone(),
        });
    }
}

fn event_family(event: ScheduledMacroEvent) -> IngressEventFamily {
    if matches!(
        event.role,
        EventRole::BallisticCrack | EventRole::BallisticBlast
    ) {
        IngressEventFamily::FiniteBallistic {
            atomic_group_id: event.atomic_group_id,
        }
    } else {
        IngressEventFamily::General {
            atomic_group_id: event.atomic_group_id,
        }
    }
}

fn final_local_leg(
    event: ScheduledMacroEvent,
    context: IngressArrivalContext<'_>,
) -> (FinalLocalLeg, bool) {
    let offset = EnuVector3::new(
        event.ingress_position_enu.east_m - context.listener_position_enu.east_m,
        event.ingress_position_enu.north_m - context.listener_position_enu.north_m,
        event.ingress_position_enu.up_m - context.listener_position_enu.up_m,
    );
    let distance_m = vector_length(offset);
    let direction = normalized(event.remote_bearing_enu);
    let delay_frames =
        f64::from(distance_m) / MACRO_SPEED_OF_SOUND_MPS * f64::from(context.sample_rate_hz);
    let valid = context.sample_rate_hz > 0
        && distance_m.is_finite()
        && direction.is_some()
        && delay_frames.is_finite()
        && delay_frames >= 0.0
        && delay_frames <= u64::MAX as f64;
    let atmosphere_gain_db = if valid {
        context
            .atmosphere
            .stage_gain_db_at_distance(distance_m)
            .unwrap_or([0.0; SPECTRAL_BAND_COUNT])
    } else {
        [0.0; SPECTRAL_BAND_COUNT]
    };
    (
        FinalLocalLeg {
            ingress_proxy_enu: event.ingress_position_enu,
            remote_direction_enu: direction.unwrap_or(EnuVector3::new(0.0, 1.0, 0.0)),
            distance_m: if distance_m.is_finite() {
                distance_m
            } else {
                0.0
            },
            delay_frames: if valid {
                delay_frames.round() as u64
            } else {
                0
            },
            distance_gain: if distance_m.is_finite() {
                1.0 / distance_m.max(1.0)
            } else {
                1.0
            },
            atmosphere_gain_db,
        },
        valid,
    )
}

fn resolve_authority(
    selected_cell: &CellIdentity,
    published: Option<&Arc<LocalCellAuthority>>,
    eligibility: EventPropagationEligibility,
) -> Result<Arc<LocalCellAuthority>, IngressFallbackReason> {
    let Some(authority) = published else {
        return Err(IngressFallbackReason::MissingActiveCell);
    };
    if authority.cell != *selected_cell || authority.world_generation == 0 {
        return Err(IngressFallbackReason::StaleActiveCell);
    }
    require_artifact(
        authority.package.as_ref(),
        authority,
        IngressFallbackReason::MissingPackageAuthority,
        IngressFallbackReason::StalePackageAuthority,
    )?;
    if eligibility.baked_reflections {
        require_artifact(
            authority.probe_bake.as_ref(),
            authority,
            IngressFallbackReason::MissingProbeBakeAuthority,
            IngressFallbackReason::StaleProbeBakeAuthority,
        )?;
    }
    if eligibility.authored_echo {
        require_artifact(
            authority.echo_authority.as_ref(),
            authority,
            IngressFallbackReason::MissingEchoAuthority,
            IngressFallbackReason::StaleEchoAuthority,
        )?;
    }
    Ok(authority.clone())
}

fn require_artifact(
    artifact: Option<&CellArtifactIdentity>,
    authority: &LocalCellAuthority,
    missing: IngressFallbackReason,
    stale: IngressFallbackReason,
) -> Result<(), IngressFallbackReason> {
    let Some(artifact) = artifact else {
        return Err(missing);
    };
    if !artifact.is_current_for(&authority.cell, authority.world_generation) {
        return Err(stale);
    }
    Ok(())
}

fn vector_length(vector: EnuVector3) -> f32 {
    let east = f64::from(vector.east_m);
    let north = f64::from(vector.north_m);
    let up = f64::from(vector.up_m);
    (east * east + north * north + up * up).sqrt() as f32
}

fn normalized(vector: EnuVector3) -> Option<EnuVector3> {
    let length = vector_length(vector);
    (length.is_finite() && length > 0.0).then(|| {
        EnuVector3::new(
            vector.east_m / length,
            vector.north_m / length,
            vector.up_m / length,
        )
    })
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FALLBACK_ATMOSPHERE_OBSERVATION;

    fn cell(name: &str) -> CellIdentity {
        CellIdentity::new("chi", name)
    }

    fn artifact(cell: &CellIdentity, generation: u64, digit: char) -> CellArtifactIdentity {
        CellArtifactIdentity::new(cell.clone(), generation, digit.to_string().repeat(64))
    }

    fn authority(cell: CellIdentity, generation: u64) -> LocalCellAuthority {
        LocalCellAuthority {
            cell: cell.clone(),
            world_generation: generation,
            package: Some(artifact(&cell, generation, '1')),
            probe_bake: Some(artifact(&cell, generation, '2')),
            echo_authority: Some(artifact(&cell, generation, '3')),
        }
    }

    fn event(
        id: u64,
        role: EventRole,
        activation_frame: u64,
        seek_frame: u64,
    ) -> ScheduledMacroEvent {
        ScheduledMacroEvent {
            event_id: MacroEventId(id),
            atomic_group_id: 77,
            role,
            asset_key: 1_000 + id,
            emission_frame: 12,
            ingress_activation_frame: activation_frame,
            program_seek_frame: seek_frame,
            tail_deadline_frame: activation_frame + 300,
            ingress_position_enu: EnuVector3::new(600.0, 0.0, 0.0),
            remote_bearing_enu: EnuVector3::new(1.0, 0.0, 0.0),
            macro_distance_gain: 0.05,
            macro_atmosphere_gain_db: [-4.0; SPECTRAL_BAND_COUNT],
            echo_anchor_key: [0; 16],
        }
    }

    fn context<'a>(
        current_frame: u64,
        atmosphere: &'a FrozenAtmosphere,
    ) -> IngressArrivalContext<'a> {
        IngressArrivalContext {
            current_frame,
            sample_rate_hz: 48_000,
            listener_position_enu: EnuVector3::default(),
            atmosphere,
        }
    }

    #[test]
    fn token_preview_is_side_effect_free_and_commit_installs_tail_once() {
        let atmosphere = FrozenAtmosphere::freeze(Some(FALLBACK_ATMOSPHERE_OBSERVATION));
        let selected = cell("e0:n0");
        let scheduled = event(91, EventRole::StandardImpulse, 100, 33);

        let mut discarded =
            MacroLocalIngress::new(selected.clone(), Some(authority(selected.clone(), 1)));
        discarded.admit_group(&[scheduled]).unwrap();
        let reservation = discarded.reserve_next_activation(500);
        let preview = discarded
            .preview_reserved(reservation, context(100, &atmosphere))
            .unwrap();
        assert_eq!(preview.len(), 1);
        assert_eq!(discarded.queued_len(), 1);
        assert!(
            discarded
                .admitted_tail(EventRole::StandardImpulse)
                .is_none()
        );
        assert_eq!(discarded.telemetry().detailed_activation_count, 0);
        assert!(discarded.activate_due(context(100, &atmosphere)).is_empty());
        drop(preview);
        discarded.discard_reserved(reservation).unwrap();
        let ordinary = discarded.activate_due(context(100, &atmosphere));
        assert_eq!(ordinary.len(), 1);
        assert_eq!(discarded.telemetry().detailed_activation_count, 1);

        let mut committed = MacroLocalIngress::new(selected.clone(), Some(authority(selected, 2)));
        committed.admit_group(&[scheduled]).unwrap();
        let reservation = committed.reserve_next_activation(500);
        assert_eq!(
            committed.preview_reserved(reservation, context(99, &atmosphere)),
            Err(EventReservationError::FrameMismatch)
        );
        let preview = committed
            .preview_reserved(reservation, context(100, &atmosphere))
            .unwrap();
        assert_eq!(committed.telemetry().detailed_activation_count, 0);
        let activation = committed
            .commit_reserved(reservation, preview, 100)
            .unwrap();
        assert_eq!(activation.len(), 1);
        assert_eq!(committed.queued_len(), 0);
        assert_eq!(committed.telemetry().detailed_activation_count, 1);
        assert_eq!(
            committed
                .admitted_tail(EventRole::StandardImpulse)
                .unwrap()
                .event_id,
            MacroEventId(91)
        );
        assert_eq!(
            committed.discard_reserved(reservation),
            Err(EventReservationError::IdentityMismatch)
        );
        assert_eq!(committed.telemetry().detailed_activation_count, 1);
    }

    #[test]
    fn event_flow_pins_old_tail_links_ballistics_and_never_silently_drops_fallbacks() {
        let atmosphere = FrozenAtmosphere::freeze(Some(FALLBACK_ATMOSPHERE_OBSERVATION));
        let cell_a = cell("e0:n0");
        let mut ingress =
            MacroLocalIngress::new(cell_a.clone(), Some(authority(cell_a.clone(), 1)));
        ingress
            .admit_group(&[
                event(101, EventRole::BallisticCrack, 100, 9),
                event(102, EventRole::BallisticBlast, 150, 17),
            ])
            .unwrap();

        let crack_batch = ingress.activate_due(context(100, &atmosphere));
        let crack = crack_batch.for_role(EventRole::BallisticCrack).unwrap();
        assert_eq!(crack.event_id, MacroEventId(101));
        assert_eq!(crack.family.atomic_group_id(), 77);
        assert_eq!(crack.emission_frame, 12);
        assert_eq!(crack.program_seek_frame, 9);
        assert_eq!(crack.macro_conditioning.arrival_frame(), 100);
        assert_eq!(
            crack.macro_conditioning.distance_gain().to_bits(),
            0.05_f32.to_bits()
        );
        assert_eq!(
            crack.macro_conditioning.atmosphere_gain_db(),
            &[-4.0; SPECTRAL_BAND_COUNT]
        );
        assert_eq!(
            crack.local_leg.ingress_proxy_enu,
            EnuVector3::new(600.0, 0.0, 0.0)
        );
        assert_eq!(
            crack.local_leg.remote_direction_enu,
            EnuVector3::new(1.0, 0.0, 0.0)
        );
        assert_eq!(crack.local_leg.delay_frames, 83_965);
        assert!(!crack.eligibility.statistical_ground);
        assert!(!crack.eligibility.baked_reflections);
        assert!(!crack.eligibility.authored_echo);
        assert!(!crack.eligibility.shared_diffuse);
        match &crack.render_authority {
            IngressRenderAuthority::DetailedLocal { authority } => {
                assert_eq!(authority.cell, cell_a)
            }
            other => panic!("crack did not enter detailed direct propagation: {other:?}"),
        }

        let cell_b = cell("e1:n0");
        let mut stale_echo = authority(cell_b.clone(), 2);
        stale_echo.echo_authority = Some(artifact(&cell_b, 1, '4'));
        ingress.publish_active_cell(cell_b.clone(), Some(stale_echo));
        let pinned = ingress.admitted_tail(EventRole::BallisticCrack).unwrap();
        match &pinned.render_authority {
            IngressRenderAuthority::DetailedLocal { authority } => {
                assert_eq!(authority.cell, cell_a)
            }
            other => panic!("cell swap stole the crack tail: {other:?}"),
        }

        let blast_batch = ingress.activate_due(context(150, &atmosphere));
        let blast = blast_batch.for_role(EventRole::BallisticBlast).unwrap();
        assert_eq!(blast.family, crack.family);
        assert_eq!(blast.program_seek_frame, 17);
        assert!(blast.eligibility.baked_reflections);
        assert!(blast.eligibility.authored_echo);
        assert_eq!(
            blast.render_authority,
            IngressRenderAuthority::MacroFallback {
                observed_authority: ingress.published_authority.clone(),
                reason: IngressFallbackReason::StaleEchoAuthority,
                shared_diffuse: true,
            }
        );
        assert_eq!(blast.fallback_ear_arrival_frame(), Some(84_115));
        assert!(ingress.activate_due(context(150, &atmosphere)).is_empty());

        ingress.publish_active_cell(cell("e2:n0"), None);
        let mut standard = event(103, EventRole::StandardImpulse, 500, 21);
        standard.atomic_group_id = 88;
        standard.tail_deadline_frame = 800;
        ingress.admit_group(&[standard]).unwrap();
        let missing_batch = ingress.activate_due(context(500, &atmosphere));
        let missing = missing_batch.for_role(EventRole::StandardImpulse).unwrap();
        assert!(matches!(
            missing.render_authority,
            IngressRenderAuthority::MacroFallback {
                reason: IngressFallbackReason::MissingActiveCell,
                shared_diffuse: true,
                ..
            }
        ));
        assert_eq!(ingress.queued_len(), 0);
        let telemetry = ingress.telemetry();
        assert_eq!(telemetry.detailed_activation_count, 1);
        assert_eq!(telemetry.fallback_activation_count, 2);
        assert_eq!(telemetry.last_fallback_event, Some(MacroEventId(103)));
        assert_eq!(
            telemetry.last_fallback_reason,
            Some(IngressFallbackReason::MissingActiveCell)
        );
    }
}
