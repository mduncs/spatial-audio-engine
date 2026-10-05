//! City-scale path partitioning and the single deterministic transient queue.
//!
//! Remote propagation is represented as a compact control-rate plan. PCM delay
//! lines remain local: dormant events wait in the fixed queue until their
//! listener-relative ingress activation, then enter one of four retained roles.

use fightbox_api::EnuVector3;
use fightbox_api::macro_transport::{
    EventRole, MacroAssetTransport, MacroEmitter, MacroEventId, MacroListener, MacroTransportConfig,
};
use fightbox_api::spectral::{
    SPECTRAL_BAND_COUNT, SpectralStage, SpectralTransfer, SpectralTransferError,
};

use crate::{AtmosphereTransferError, FrozenAtmosphere};

pub const MACRO_SPEED_OF_SOUND_MPS: f64 = 343.0;
pub const MACRO_EVENT_QUEUE_CAPACITY: usize = 1_024;
pub const MAX_ATOMIC_EVENT_GROUP: usize = EventRole::COUNT;

/// One physically disjoint section of a remote-to-ear path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacroPropagationSegment {
    pub distance_m: f64,
    pub delay_s: f64,
    /// Incremental inverse-distance gain assigned to this section.
    pub distance_gain: f32,
    pub atmosphere_gain_db: [f32; SPECTRAL_BAND_COUNT],
}

/// How motion encoded in the source asset reaches presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroMotionPresentation {
    DrySourceMotion,
    StaticProxyRecordingCarriesMotion,
}

/// Complete listener-relative partition for one real remote emitter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacroTransportPlan {
    pub emitter_id: MacroEventId,
    pub ingress_position_enu: EnuVector3,
    /// Unit direction from the listener toward the real emitter and ingress.
    pub remote_bearing_enu: EnuVector3,
    pub total_distance_m: f64,
    pub total_delay_s: f64,
    pub macro_segment: MacroPropagationSegment,
    pub local_segment: MacroPropagationSegment,
    pub total_atmosphere_gain_db: [f32; SPECTRAL_BAND_COUNT],
    /// Program position heard at the listener's planning epoch. `None` means
    /// the emission has not reached this listener yet.
    pub program_seek_s: Option<f64>,
    pub motion_presentation: MacroMotionPresentation,
}

/// Queue-facing identity and lifecycle for one planned ingress event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacroEventScheduleRequest {
    pub event_id: MacroEventId,
    pub atomic_group_id: u64,
    pub role: EventRole,
    pub asset_key: u64,
    pub emission_frame: u64,
    pub program_seek_frame: u64,
    pub retained_frames_after_activation: u64,
    pub sample_rate_hz: u32,
}

impl MacroTransportPlan {
    /// Publishes the sum of both physical atmosphere segments exactly once.
    pub fn publish_atmosphere(
        &self,
        transfer: &mut SpectralTransfer,
    ) -> Result<(), SpectralTransferError> {
        transfer.set_stage(SpectralStage::Atmosphere, self.total_atmosphere_gain_db)
    }

    /// Session time at which a one-shot should activate its local ingress.
    #[must_use]
    pub fn ingress_activation_time_s(&self, emission_time_s: f64) -> f64 {
        emission_time_s + self.macro_segment.delay_s
    }

    /// Product of the independently assigned segment gains.
    #[must_use]
    pub fn composed_distance_gain(&self) -> f32 {
        self.macro_segment.distance_gain * self.local_segment.distance_gain
    }

    /// Converts the macro segment into a compact dormant event. The local
    /// renderer still contributes `local_segment.delay_s`, so the final ear
    /// arrival remains the sum of the two physical sections.
    pub fn schedule_event(
        &self,
        request: MacroEventScheduleRequest,
    ) -> Result<ScheduledMacroEvent, EventAdmissionError> {
        if request.sample_rate_hz == 0 {
            return Err(EventAdmissionError::InvalidSampleRate);
        }
        let macro_delay_frames =
            (self.macro_segment.delay_s * f64::from(request.sample_rate_hz)).round();
        if !macro_delay_frames.is_finite()
            || macro_delay_frames < 0.0
            || macro_delay_frames > u64::MAX as f64
        {
            return Err(EventAdmissionError::InvalidTimeline);
        }
        let ingress_activation_frame = request
            .emission_frame
            .checked_add(macro_delay_frames as u64)
            .ok_or(EventAdmissionError::InvalidTimeline)?;
        let tail_deadline_frame = ingress_activation_frame
            .checked_add(request.retained_frames_after_activation)
            .ok_or(EventAdmissionError::InvalidTimeline)?;
        let event = ScheduledMacroEvent {
            event_id: request.event_id,
            atomic_group_id: request.atomic_group_id,
            role: request.role,
            asset_key: request.asset_key,
            emission_frame: request.emission_frame,
            ingress_activation_frame,
            program_seek_frame: request.program_seek_frame,
            tail_deadline_frame,
            ingress_position_enu: self.ingress_position_enu,
            remote_bearing_enu: self.remote_bearing_enu,
            macro_distance_gain: self.macro_segment.distance_gain,
            macro_atmosphere_gain_db: self.macro_segment.atmosphere_gain_db,
            echo_anchor_key: [0; 16],
        };
        event.validate()?;
        Ok(event)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacroTransportError {
    InvalidEmitterId,
    UnsupportedLiveAsset,
    NonFiniteInput,
    InvalidLocalHorizon,
    CoincidentEmitterAndListener,
    Atmosphere(AtmosphereTransferError),
}

/// Builds one macro/local split without allocating or retaining audio history.
pub fn plan_macro_transport(
    emitter: MacroEmitter,
    listener: MacroListener,
    config: MacroTransportConfig,
    atmosphere: &FrozenAtmosphere,
) -> Result<MacroTransportPlan, MacroTransportError> {
    if emitter.id.0 == 0 {
        return Err(MacroTransportError::InvalidEmitterId);
    }
    if emitter.asset_transport == MacroAssetTransport::NonSeekableLive {
        return Err(MacroTransportError::UnsupportedLiveAsset);
    }
    if !emitter.position_enu.is_finite()
        || !listener.position_enu.is_finite()
        || !emitter.program_started_at_s.is_finite()
        || !listener.session_time_s.is_finite()
    {
        return Err(MacroTransportError::NonFiniteInput);
    }
    if !config.local_horizon_m.is_finite() || config.local_horizon_m < 1.0 {
        return Err(MacroTransportError::InvalidLocalHorizon);
    }

    let offset = subtract(emitter.position_enu, listener.position_enu);
    let total_distance_m = length_f64(offset);
    if !total_distance_m.is_finite() || total_distance_m <= 0.0 {
        return Err(MacroTransportError::CoincidentEmitterAndListener);
    }
    let bearing = scale(offset, (1.0 / total_distance_m) as f32);
    let local_distance_m = total_distance_m.min(f64::from(config.local_horizon_m));
    let macro_distance_m = total_distance_m - local_distance_m;
    let ingress_position_enu = add(
        listener.position_enu,
        scale(bearing, local_distance_m as f32),
    );

    // Inverse-distance attenuation is not multiplicative by raw segment
    // lengths. Let the local backend apply its ordinary local distance, then
    // assign Macro the remaining ratio so their product is exactly the one
    // end-to-end law rather than a double attenuation.
    let total_distance_gain = inverse_distance_gain(total_distance_m);
    let local_distance_gain = inverse_distance_gain(local_distance_m);
    let macro_distance_gain = total_distance_gain / local_distance_gain;
    let macro_atmosphere = atmosphere
        .stage_gain_db_at_distance(macro_distance_m as f32)
        .map_err(MacroTransportError::Atmosphere)?;
    let local_atmosphere = atmosphere
        .stage_gain_db_at_distance(local_distance_m as f32)
        .map_err(MacroTransportError::Atmosphere)?;
    // Produce the callback-facing total directly from the physical distance so
    // very large upper-band losses do not accumulate two intermediate `f32`
    // roundings. The two segment stems remain available for authority evidence.
    let total_atmosphere_gain_db = atmosphere
        .stage_gain_db_at_distance(total_distance_m as f32)
        .map_err(MacroTransportError::Atmosphere)?;
    let total_delay_s = total_distance_m / MACRO_SPEED_OF_SOUND_MPS;
    let emitted_program_time_s =
        listener.session_time_s - total_delay_s - emitter.program_started_at_s;

    Ok(MacroTransportPlan {
        emitter_id: emitter.id,
        ingress_position_enu,
        remote_bearing_enu: bearing,
        total_distance_m,
        total_delay_s,
        macro_segment: MacroPropagationSegment {
            distance_m: macro_distance_m,
            delay_s: macro_distance_m / MACRO_SPEED_OF_SOUND_MPS,
            distance_gain: macro_distance_gain,
            atmosphere_gain_db: macro_atmosphere,
        },
        local_segment: MacroPropagationSegment {
            distance_m: local_distance_m,
            delay_s: local_distance_m / MACRO_SPEED_OF_SOUND_MPS,
            distance_gain: local_distance_gain,
            atmosphere_gain_db: local_atmosphere,
        },
        total_atmosphere_gain_db,
        program_seek_s: (emitted_program_time_s >= 0.0).then_some(emitted_program_time_s),
        motion_presentation: if emitter.recording_carries_motion {
            MacroMotionPresentation::StaticProxyRecordingCarriesMotion
        } else {
            MacroMotionPresentation::DrySourceMotion
        },
    })
}

fn inverse_distance_gain(distance_m: f64) -> f32 {
    (1.0 / distance_m.max(1.0)) as f32
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

fn scale(vector: EnuVector3, scalar: f32) -> EnuVector3 {
    EnuVector3::new(
        vector.east_m * scalar,
        vector.north_m * scalar,
        vector.up_m * scalar,
    )
}

fn length_f64(vector: EnuVector3) -> f64 {
    let east = f64::from(vector.east_m);
    let north = f64::from(vector.north_m);
    let up = f64::from(vector.up_m);
    (east * east + north * north + up * up).sqrt()
}

/// Compact dormant event record. Asset identity is a manifest hash/index key,
/// never a heap-owned path or decoded program.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScheduledMacroEvent {
    pub event_id: MacroEventId,
    pub atomic_group_id: u64,
    pub role: EventRole,
    pub asset_key: u64,
    pub emission_frame: u64,
    pub ingress_activation_frame: u64,
    pub program_seek_frame: u64,
    pub tail_deadline_frame: u64,
    pub ingress_position_enu: EnuVector3,
    pub remote_bearing_enu: EnuVector3,
    pub macro_distance_gain: f32,
    pub macro_atmosphere_gain_db: [f32; SPECTRAL_BAND_COUNT],
    /// Explicit authored static ingress anchor; all-zero means structural echo Off.
    pub echo_anchor_key: [u8; 16],
}

impl Default for ScheduledMacroEvent {
    fn default() -> Self {
        Self {
            event_id: MacroEventId(0),
            atomic_group_id: 0,
            role: EventRole::CinematicImpulse,
            asset_key: 0,
            emission_frame: 0,
            ingress_activation_frame: 0,
            program_seek_frame: 0,
            tail_deadline_frame: 0,
            ingress_position_enu: EnuVector3::default(),
            remote_bearing_enu: EnuVector3::new(0.0, 1.0, 0.0),
            macro_distance_gain: 0.0,
            macro_atmosphere_gain_db: [0.0; SPECTRAL_BAND_COUNT],
            echo_anchor_key: [0; 16],
        }
    }
}

impl ScheduledMacroEvent {
    fn validate(self) -> Result<(), EventAdmissionError> {
        if self.event_id.0 == 0 || self.atomic_group_id == 0 {
            return Err(EventAdmissionError::ZeroIdentity);
        }
        if self.asset_key == 0 {
            return Err(EventAdmissionError::ZeroAssetKey);
        }
        if self.ingress_activation_frame < self.emission_frame
            || self.tail_deadline_frame < self.ingress_activation_frame
        {
            return Err(EventAdmissionError::InvalidTimeline);
        }
        if !self.ingress_position_enu.is_finite()
            || !self.remote_bearing_enu.is_finite()
            || !self.macro_distance_gain.is_finite()
            || self.macro_distance_gain < 0.0
            || !self
                .macro_atmosphere_gain_db
                .into_iter()
                .all(f32::is_finite)
        {
            return Err(EventAdmissionError::NonFiniteTransport);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct EventQueueSlot {
    occupied: bool,
    reserved: bool,
    event: ScheduledMacroEvent,
}

/// Fixed-capacity open-addressed index from raw `event_id` to its queue slot.
///
/// Capacity equals `MACRO_EVENT_QUEUE_CAPACITY`, so every admitted record is
/// guaranteed a home and the table never grows after construction. Probes are
/// linear; raw id 0 is rejected at admission and serves as the empty sentinel,
/// while removals leave a tombstone slot marker that lookups skip past and
/// insertions reclaim. Lookups are bounded by one full table sweep, so they
/// terminate even when tombstones accumulate.
struct EventIdIndex {
    entries: Box<[(u64, u32)]>,
}

impl EventIdIndex {
    const TOMBSTONE_SLOT: u32 = u32::MAX;

    fn new() -> Self {
        Self {
            entries: vec![(0_u64, 0_u32); MACRO_EVENT_QUEUE_CAPACITY].into_boxed_slice(),
        }
    }

    fn probe_start(event_id: u64) -> usize {
        (event_id.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            >> (64 - MACRO_EVENT_QUEUE_CAPACITY.trailing_zeros())) as usize
    }

    fn get(&self, event_id: u64) -> Option<usize> {
        debug_assert!(event_id != 0);
        let mask = MACRO_EVENT_QUEUE_CAPACITY - 1;
        let mut cursor = Self::probe_start(event_id);
        for _ in 0..MACRO_EVENT_QUEUE_CAPACITY {
            let (key, slot) = self.entries[cursor];
            if key == 0 {
                return None;
            }
            if key == event_id && slot != Self::TOMBSTONE_SLOT {
                return Some(slot as usize);
            }
            cursor = (cursor + 1) & mask;
        }
        None
    }

    fn insert(&mut self, event_id: u64, slot_index: usize) {
        debug_assert!(event_id != 0);
        let mask = MACRO_EVENT_QUEUE_CAPACITY - 1;
        let mut cursor = Self::probe_start(event_id);
        for _ in 0..MACRO_EVENT_QUEUE_CAPACITY {
            let (key, slot) = self.entries[cursor];
            if key == 0 || slot == Self::TOMBSTONE_SLOT {
                self.entries[cursor] = (event_id, slot_index as u32);
                return;
            }
            debug_assert!(key != event_id, "duplicate event id entered the index");
            cursor = (cursor + 1) & mask;
        }
        unreachable!("live queue entries never exceed the fixed table capacity");
    }

    fn remove(&mut self, event_id: u64, slot_index: usize) {
        debug_assert!(event_id != 0);
        let mask = MACRO_EVENT_QUEUE_CAPACITY - 1;
        let mut cursor = Self::probe_start(event_id);
        for _ in 0..MACRO_EVENT_QUEUE_CAPACITY {
            let (key, slot) = self.entries[cursor];
            if key == 0 {
                break;
            }
            if key == event_id && slot == slot_index as u32 {
                self.entries[cursor] = (key, Self::TOMBSTONE_SLOT);
                return;
            }
            cursor = (cursor + 1) & mask;
        }
        debug_assert!(false, "removed an event id that was never indexed");
    }
}

/// The one V1 dormant-event queue. Admission mutates it only after the entire
/// group validates, so crack/blast pairs never enter partially.
pub struct MacroEventQueue {
    // The fixed 1,024-record store is control-owned but too large to
    // materialize repeatedly on the small host/session-construction stack.
    // A boxed slice keeps the exact fixed capacity and performs no allocation
    // after construction.
    slots: Box<[EventQueueSlot]>,
    id_index: EventIdIndex,
    len: u16,
}

impl MacroEventQueue {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: vec![EventQueueSlot::default(); MACRO_EVENT_QUEUE_CAPACITY].into_boxed_slice(),
            id_index: EventIdIndex::new(),
            len: 0,
        }
    }

    #[must_use]
    pub fn persistent_bytes(&self) -> usize {
        core::mem::size_of::<Self>()
            + core::mem::size_of_val(self.slots.as_ref())
            + core::mem::size_of_val(self.id_index.entries.as_ref())
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Atomically admits one singleton or multi-role group.
    pub fn admit_group(
        &mut self,
        events: &[ScheduledMacroEvent],
    ) -> Result<(), EventAdmissionError> {
        if events.is_empty() || events.len() > MAX_ATOMIC_EVENT_GROUP {
            return Err(EventAdmissionError::InvalidGroupSize);
        }
        if self.len().saturating_add(events.len()) > MACRO_EVENT_QUEUE_CAPACITY {
            return Err(EventAdmissionError::QueueFull);
        }
        let group_id = events[0].atomic_group_id;
        let mut roles_seen = [false; EventRole::COUNT];
        for (index, event) in events.iter().copied().enumerate() {
            event.validate()?;
            if event.atomic_group_id != group_id {
                return Err(EventAdmissionError::MixedGroupIdentity);
            }
            if roles_seen[event.role.index()] {
                return Err(EventAdmissionError::DuplicateRoleInGroup);
            }
            roles_seen[event.role.index()] = true;
            if self.contains_id(event.event_id)
                || events[..index]
                    .iter()
                    .any(|prior| prior.event_id == event.event_id)
            {
                return Err(EventAdmissionError::DuplicateEventId);
            }
        }

        let mut event_index = 0;
        for (slot_index, slot) in self.slots.iter_mut().enumerate() {
            if !slot.occupied {
                *slot = EventQueueSlot {
                    occupied: true,
                    reserved: false,
                    event: events[event_index],
                };
                self.id_index
                    .insert(events[event_index].event_id.0, slot_index);
                event_index += 1;
                if event_index == events.len() {
                    break;
                }
            }
        }
        self.len += events.len() as u16;
        Ok(())
    }

    #[must_use]
    pub fn contains_id(&self, event_id: MacroEventId) -> bool {
        event_id.0 != 0 && self.id_index.get(event_id.0).is_some()
    }

    /// Binds a dormant event to one authored static ingress anchor.
    pub fn bind_echo_anchor(
        &mut self,
        event_id: MacroEventId,
        anchor_key: [u8; 16],
    ) -> Result<(), EventEchoBindingError> {
        if event_id.0 == 0 || anchor_key == [0; 16] {
            return Err(EventEchoBindingError::ZeroIdentity);
        }
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.occupied && slot.event.event_id == event_id)
        else {
            return Err(EventEchoBindingError::EventNotDormant);
        };
        if slot.reserved {
            return Err(EventEchoBindingError::EventReserved);
        }
        if !slot
            .event
            .role
            .local_propagation_eligibility()
            .authored_echo
        {
            return Err(EventEchoBindingError::IneligibleRole);
        }
        slot.event.echo_anchor_key = anchor_key;
        Ok(())
    }

    fn earliest_due_for_available_roles(
        &self,
        current_frame: u64,
        roles_available: [bool; EventRole::COUNT],
    ) -> Option<usize> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                slot.occupied
                    && !slot.reserved
                    && slot.event.ingress_activation_frame <= current_frame
                    && roles_available[slot.event.role.index()]
            })
            .min_by_key(|(_, slot)| {
                (
                    slot.event.ingress_activation_frame,
                    slot.event.atomic_group_id,
                    slot.event.role,
                    slot.event.event_id,
                )
            })
            .map(|(index, _)| index)
    }

    fn remove(&mut self, index: usize) -> ScheduledMacroEvent {
        let event = self.slots[index].event;
        self.id_index.remove(event.event_id.0, index);
        self.slots[index].occupied = false;
        self.len -= 1;
        event
    }
}

impl Default for MacroEventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventAdmissionError {
    InvalidGroupSize,
    QueueFull,
    ZeroIdentity,
    ZeroAssetKey,
    InvalidSampleRate,
    InvalidTimeline,
    NonFiniteTransport,
    MixedGroupIdentity,
    DuplicateRoleInGroup,
    DuplicateEventId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ActiveEventVoice {
    event_id: MacroEventId,
    tail_deadline_frame: u64,
}

/// Fixed activation result with no callback-time allocation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EventActivationBatch {
    pub count: u8,
    pub events: [ScheduledMacroEvent; EventRole::COUNT],
}

impl Default for EventActivationBatch {
    fn default() -> Self {
        Self {
            count: 0,
            events: [ScheduledMacroEvent::default(); EventRole::COUNT],
        }
    }
}

/// Fixed scalar preview of queue records held dormant for one future common
/// activation frame. Reserved records remain in the 1,024-slot queue and count
/// against capacity until commit or discard.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EventReservationBatch {
    pub count: u8,
    pub events: [ScheduledMacroEvent; EventRole::COUNT],
}

impl EventReservationBatch {
    #[must_use]
    pub const fn len(self) -> usize {
        self.count as usize
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &ScheduledMacroEvent> {
        self.events[..self.count as usize].iter()
    }

    #[must_use]
    pub fn activation_frame(self) -> Option<u64> {
        (!self.is_empty()).then_some(self.events[0].ingress_activation_frame)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventEchoBindingError {
    ZeroIdentity,
    EventNotDormant,
    EventReserved,
    IneligibleRole,
}

/// Invalid mutation of a dormant reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventReservationError {
    Empty,
    IdentityMismatch,
    FrameMismatch,
    RoleBusy,
}

/// Queue plus four role-pinned live reservations.
pub struct MacroEventScheduler {
    queue: MacroEventQueue,
    voices: [Option<ActiveEventVoice>; EventRole::COUNT],
}

impl MacroEventScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: MacroEventQueue::new(),
            voices: [None; EventRole::COUNT],
        }
    }

    pub fn admit_group(
        &mut self,
        events: &[ScheduledMacroEvent],
    ) -> Result<(), EventAdmissionError> {
        self.queue.admit_group(events)
    }

    pub fn bind_echo_anchor(
        &mut self,
        event_id: MacroEventId,
        anchor_key: [u8; 16],
    ) -> Result<(), EventEchoBindingError> {
        self.queue.bind_echo_anchor(event_id, anchor_key)
    }

    #[must_use]
    pub const fn queued_len(&self) -> usize {
        self.queue.len()
    }

    /// Marks the earliest eligible common activation frame at or before the
    /// lookahead horizon without consuming queue capacity or occupying voices.
    /// Only one reservation batch may exist in this V1 scheduler.
    pub fn reserve_next_activation(&mut self, lookahead_frame: u64) -> EventReservationBatch {
        #[derive(Clone, Copy)]
        struct RoleCandidate {
            role: EventRole,
            slot_index: usize,
            activation_frame: u64,
            atomic_group_id: u64,
            event_id: MacroEventId,
        }
        let mut target_frame: Option<u64> = None;
        let mut any_reserved = false;
        let mut best_per_role = [const { None }; EventRole::COUNT];
        for (slot_index, slot) in self.queue.slots.iter().enumerate() {
            if !slot.occupied {
                continue;
            }
            if slot.reserved {
                any_reserved = true;
                continue;
            }
            let activation_frame = slot.event.ingress_activation_frame;
            if activation_frame > lookahead_frame
                || self.voices[slot.event.role.index()]
                    .is_some_and(|voice| voice.tail_deadline_frame > activation_frame)
            {
                continue;
            }
            // Reproduce the pre-optimization per-round rescan in one pass:
            // only events sharing the minimum eligible activation frame ever
            // compete. A strictly earlier frame discards every candidate
            // tracked so far (the original rescan restricted each round to
            // `ingress_activation_frame == target_frame`, so later-frame
            // candidates could never be reserved), and events on the current
            // minimum compare by the original key, which reduces to
            // `(atomic_group_id, event_id)` because the role is fixed here.
            match target_frame {
                Some(current) if activation_frame > current => continue,
                Some(current) if activation_frame == current => {}
                _ => {
                    target_frame = Some(activation_frame);
                    best_per_role = [const { None }; EventRole::COUNT];
                }
            }
            let role_index = slot.event.role.index();
            let candidate_key = (slot.event.atomic_group_id, slot.event.event_id.0);
            let replaces_best = best_per_role[role_index].is_none_or(|current: RoleCandidate| {
                candidate_key < (current.atomic_group_id, current.event_id.0)
            });
            if replaces_best {
                best_per_role[role_index] = Some(RoleCandidate {
                    role: slot.event.role,
                    slot_index,
                    activation_frame,
                    atomic_group_id: slot.event.atomic_group_id,
                    event_id: slot.event.event_id,
                });
            }
        }
        if any_reserved {
            return EventReservationBatch::default();
        }
        let Some(target_frame) = target_frame else {
            return EventReservationBatch::default();
        };

        // One event per role, emitted in the exact order the previous
        // per-round rescan produced: successive minima of
        // (atomic_group_id, role, event_id) over the roles whose best
        // candidate sits on the common activation frame.
        let mut output = EventReservationBatch::default();
        loop {
            let next = best_per_role
                .iter_mut()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate
                        .as_ref()
                        .is_some_and(|picked| picked.activation_frame == target_frame)
                })
                .min_by_key(|(_, candidate)| {
                    let picked = candidate
                        .as_ref()
                        .expect("filter keeps only occupied candidates");
                    (picked.atomic_group_id, picked.role, picked.event_id.0)
                })
                .map(|(role_index, _)| role_index);
            let Some(role_index) = next else {
                break;
            };
            let picked = best_per_role[role_index]
                .take()
                .expect("selected candidates are only consumed once");
            self.queue.slots[picked.slot_index].reserved = true;
            output.events[usize::from(output.count)] = self.queue.slots[picked.slot_index].event;
            output.count += 1;
            if usize::from(output.count) == EventRole::COUNT {
                break;
            }
        }
        output
    }

    /// Verifies that every scalar record still names a held dormant queue slot.
    pub fn validate_reserved(
        &self,
        reservation: EventReservationBatch,
    ) -> Result<(), EventReservationError> {
        if reservation.is_empty() {
            return Err(EventReservationError::Empty);
        }
        let Some(frame) = reservation.activation_frame() else {
            return Err(EventReservationError::Empty);
        };
        for event in reservation.iter() {
            if event.ingress_activation_frame != frame
                || !self
                    .queue
                    .slots
                    .iter()
                    .any(|slot| slot.occupied && slot.reserved && slot.event == *event)
            {
                return Err(EventReservationError::IdentityMismatch);
            }
        }
        Ok(())
    }

    /// Proves that the exact reserved batch can commit at `current_frame`
    /// without mutating queue or voice state. A voice whose deadline is this
    /// frame is treated as due because commit retires it before installation.
    pub fn validate_reserved_commit(
        &self,
        reservation: EventReservationBatch,
        current_frame: u64,
    ) -> Result<(), EventReservationError> {
        self.validate_reserved(reservation)?;
        if reservation.activation_frame() != Some(current_frame)
            || reservation
                .iter()
                .any(|event| event.ingress_activation_frame != current_frame)
        {
            return Err(EventReservationError::FrameMismatch);
        }
        if reservation.iter().any(|event| {
            self.voices[event.role.index()]
                .is_some_and(|voice| voice.tail_deadline_frame > current_frame)
        }) {
            return Err(EventReservationError::RoleBusy);
        }
        Ok(())
    }

    /// Consumes exactly one previously reserved batch at its exact activation
    /// frame and occupies its role voices. Validation completes before mutation.
    pub fn commit_reserved(
        &mut self,
        reservation: EventReservationBatch,
        current_frame: u64,
    ) -> Result<EventActivationBatch, EventReservationError> {
        self.validate_reserved_commit(reservation, current_frame)?;
        self.retire_due_voices(current_frame);

        let mut output = EventActivationBatch::default();
        for event in reservation.iter().copied() {
            let index = self
                .queue
                .slots
                .iter()
                .position(|slot| slot.occupied && slot.reserved && slot.event == event)
                .expect("the reservation was validated before mutation");
            let event = self.queue.remove(index);
            self.voices[event.role.index()] = Some(ActiveEventVoice {
                event_id: event.event_id,
                tail_deadline_frame: event.tail_deadline_frame,
            });
            output.events[usize::from(output.count)] = event;
            output.count += 1;
        }
        Ok(output)
    }

    /// Returns a matching pre-commit reservation to the dormant set. Repeating
    /// the same discard while records remain queued is idempotent.
    pub fn discard_reserved(
        &mut self,
        reservation: EventReservationBatch,
    ) -> Result<(), EventReservationError> {
        if reservation.is_empty() {
            return Err(EventReservationError::Empty);
        }
        for event in reservation.iter() {
            if !self
                .queue
                .slots
                .iter()
                .any(|slot| slot.occupied && slot.event == *event)
            {
                return Err(EventReservationError::IdentityMismatch);
            }
        }
        for event in reservation.iter() {
            let slot = self
                .queue
                .slots
                .iter_mut()
                .find(|slot| slot.occupied && slot.event == *event)
                .expect("the reservation was validated before mutation");
            slot.reserved = false;
        }
        Ok(())
    }

    /// Retires completed tails, then activates at most one event per dedicated
    /// role. A busy Crack role cannot block a due Blast or steal its slot.
    pub fn activate_due(&mut self, current_frame: u64) -> EventActivationBatch {
        self.retire_due_voices(current_frame);
        let mut batch = EventActivationBatch::default();
        loop {
            let available = std::array::from_fn(|index| self.voices[index].is_none());
            let Some(index) = self
                .queue
                .earliest_due_for_available_roles(current_frame, available)
            else {
                break;
            };
            let event = self.queue.remove(index);
            self.voices[event.role.index()] = Some(ActiveEventVoice {
                event_id: event.event_id,
                tail_deadline_frame: event.tail_deadline_frame,
            });
            batch.events[usize::from(batch.count)] = event;
            batch.count += 1;
            if usize::from(batch.count) == EventRole::COUNT {
                break;
            }
        }
        batch
    }

    fn retire_due_voices(&mut self, current_frame: u64) {
        for voice in &mut self.voices {
            if voice.is_some_and(|active| active.tail_deadline_frame <= current_frame) {
                *voice = None;
            }
        }
    }

    /// Explicit energy-floor retirement before the declared deadline.
    pub fn release(
        &mut self,
        role: EventRole,
        event_id: MacroEventId,
    ) -> Result<(), EventReleaseError> {
        let slot = &mut self.voices[role.index()];
        match *slot {
            Some(active) if active.event_id == event_id => {
                *slot = None;
                Ok(())
            }
            Some(_) => Err(EventReleaseError::IdentityMismatch),
            None => Err(EventReleaseError::RoleIdle),
        }
    }

    /// Extends the role reservation after the ingress layer resolves the final
    /// listener-local delay. This keeps the role occupied through the actual
    /// ear-relative tail rather than only through arrival at the cell edge.
    pub(crate) fn extend_active_tail(
        &mut self,
        role: EventRole,
        event_id: MacroEventId,
        tail_deadline_frame: u64,
    ) -> Result<(), EventReleaseError> {
        let slot = &mut self.voices[role.index()];
        match slot {
            Some(active) if active.event_id == event_id => {
                active.tail_deadline_frame = active.tail_deadline_frame.max(tail_deadline_frame);
                Ok(())
            }
            Some(_) => Err(EventReleaseError::IdentityMismatch),
            None => Err(EventReleaseError::RoleIdle),
        }
    }

    #[must_use]
    pub fn active_event(&self, role: EventRole) -> Option<MacroEventId> {
        self.voices[role.index()].map(|voice| voice.event_id)
    }
}

impl Default for MacroEventScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventReleaseError {
    RoleIdle,
    IdentityMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FALLBACK_ATMOSPHERE_OBSERVATION;

    fn emitter(distance_m: f32) -> MacroEmitter {
        MacroEmitter {
            id: MacroEventId(1),
            position_enu: EnuVector3::new(distance_m, 0.0, 0.0),
            program_started_at_s: 0.0,
            asset_transport: MacroAssetTransport::Seekable,
            recording_carries_motion: false,
        }
    }

    fn listener(session_time_s: f64) -> MacroListener {
        MacroListener {
            position_enu: EnuVector3::default(),
            session_time_s,
        }
    }

    #[test]
    fn transport_clock_and_gain_partition_are_exact_at_city_scale() {
        let atmosphere = FrozenAtmosphere::freeze(Some(FALLBACK_ATMOSPHERE_OBSERVATION));
        let plan = plan_macro_transport(
            emitter(10_000.0),
            listener(40.0),
            MacroTransportConfig::default(),
            &atmosphere,
        )
        .unwrap();
        assert!((plan.total_delay_s - 29.154_518_950_4).abs() < 1.0e-9);
        assert!((plan.macro_segment.distance_m - 9_400.0).abs() < 1.0e-9);
        assert!((plan.local_segment.distance_m - 600.0).abs() < 1.0e-9);
        assert!((f64::from(plan.composed_distance_gain()) - 0.0001).abs() < 1.0e-10);
        assert!((f64::from(plan.remote_bearing_enu.east_m) - 1.0).abs() < 1.0e-7);
        assert!((f64::from(plan.ingress_position_enu.east_m) - 600.0).abs() < 1.0e-6);
        assert!((plan.program_seek_s.unwrap() - (40.0 - plan.total_delay_s)).abs() < 1.0e-9);
        for band_index in 0..SPECTRAL_BAND_COUNT {
            let full = atmosphere.stage_gain_db_at_distance(10_000.0).unwrap()[band_index];
            assert!((plan.total_atmosphere_gain_db[band_index] - full).abs() < 0.0001);
        }
    }

    #[test]
    fn canonical_transport_ranges_keep_one_clock_and_one_ingress_schedule() {
        let atmosphere = FrozenAtmosphere::freeze(None);
        for (distance_m, expected_arrival_s) in [
            (100.0_f32, 0.291_545_189_5),
            (1_000.0, 2.915_451_895_0),
            (10_000.0, 29.154_518_950_4),
        ] {
            let plan = plan_macro_transport(
                emitter(distance_m),
                listener(40.0),
                MacroTransportConfig::default(),
                &atmosphere,
            )
            .unwrap();
            assert!((plan.total_delay_s - expected_arrival_s).abs() < 1.0e-9);
            let scheduled = plan
                .schedule_event(MacroEventScheduleRequest {
                    event_id: MacroEventId(distance_m as u64),
                    atomic_group_id: distance_m as u64,
                    role: EventRole::CinematicImpulse,
                    asset_key: 99,
                    emission_frame: 0,
                    program_seek_frame: 0,
                    retained_frames_after_activation: 144_000,
                    sample_rate_hz: 48_000,
                })
                .unwrap();
            let local_delay_frames = (plan.local_segment.delay_s * 48_000.0).round() as u64;
            let reconstructed_arrival_s =
                (scheduled.ingress_activation_frame + local_delay_frames) as f64 / 48_000.0;
            assert!(
                (reconstructed_arrival_s - expected_arrival_s).abs() <= 1.0 / 48_000.0,
                "{distance_m} m reconstructed {reconstructed_arrival_s:.9} s"
            );
        }
    }

    #[test]
    fn near_sources_collapse_macro_to_neutral_and_live_assets_are_rejected() {
        let atmosphere = FrozenAtmosphere::freeze(None);
        let near = plan_macro_transport(
            emitter(100.0),
            listener(1.0),
            MacroTransportConfig::default(),
            &atmosphere,
        )
        .unwrap();
        assert_eq!(near.macro_segment.distance_m, 0.0);
        assert_eq!(
            near.macro_segment.distance_gain.to_bits(),
            1.0_f32.to_bits()
        );
        assert_eq!(
            near.macro_segment.atmosphere_gain_db,
            [0.0; SPECTRAL_BAND_COUNT]
        );
        assert_eq!(near.program_seek_s, Some(1.0 - 100.0 / 343.0));

        let unsupported = MacroEmitter {
            asset_transport: MacroAssetTransport::NonSeekableLive,
            ..emitter(1_000.0)
        };
        assert_eq!(
            plan_macro_transport(
                unsupported,
                listener(10.0),
                MacroTransportConfig::default(),
                &atmosphere,
            ),
            Err(MacroTransportError::UnsupportedLiveAsset)
        );
    }

    fn event(id: u64, group: u64, role: EventRole, activation: u64) -> ScheduledMacroEvent {
        ScheduledMacroEvent {
            event_id: MacroEventId(id),
            atomic_group_id: group,
            role,
            asset_key: id * 10,
            emission_frame: 0,
            ingress_activation_frame: activation,
            program_seek_frame: 0,
            tail_deadline_frame: activation + 4_800,
            ingress_position_enu: EnuVector3::new(600.0, 0.0, 0.0),
            remote_bearing_enu: EnuVector3::new(1.0, 0.0, 0.0),
            macro_distance_gain: 0.1,
            macro_atmosphere_gain_db: [-1.0; SPECTRAL_BAND_COUNT],
            echo_anchor_key: [0; 16],
        }
    }

    #[test]
    fn authored_echo_anchor_binding_is_explicit_dormant_and_reservation_safe() {
        let mut scheduler = MacroEventScheduler::new();
        scheduler
            .admit_group(&[event(19, 5, EventRole::StandardImpulse, 100)])
            .unwrap();
        assert_eq!(
            scheduler.bind_echo_anchor(MacroEventId(19), [0; 16]),
            Err(EventEchoBindingError::ZeroIdentity)
        );
        let anchor = [9_u8; 16];
        scheduler
            .bind_echo_anchor(MacroEventId(19), anchor)
            .unwrap();
        let reservation = scheduler.reserve_next_activation(100);
        assert_eq!(reservation.events[0].echo_anchor_key, anchor);
        assert_eq!(
            scheduler.bind_echo_anchor(MacroEventId(19), [8; 16]),
            Err(EventEchoBindingError::EventReserved)
        );
        scheduler.discard_reserved(reservation).unwrap();
        scheduler
            .bind_echo_anchor(MacroEventId(19), [8; 16])
            .unwrap();
        assert_eq!(
            scheduler.reserve_next_activation(100).events[0].echo_anchor_key,
            [8; 16]
        );
    }

    #[test]
    fn reservation_stays_dormant_uses_one_exact_frame_and_discards_without_reordering() {
        let shot = [
            event(20, 7, EventRole::BallisticCrack, 100),
            event(21, 7, EventRole::BallisticBlast, 150),
        ];
        let mut scheduler = MacroEventScheduler::new();
        scheduler.admit_group(&shot).unwrap();

        let crack = scheduler.reserve_next_activation(200);
        assert_eq!(crack.count, 1);
        assert_eq!(crack.activation_frame(), Some(100));
        assert_eq!(crack.events[0].event_id, MacroEventId(20));
        assert_eq!(scheduler.queued_len(), 2);
        assert_eq!(scheduler.activate_due(100).count, 0);
        scheduler.discard_reserved(crack).unwrap();
        scheduler.discard_reserved(crack).unwrap();
        let ordinary = scheduler.activate_due(100);
        assert_eq!(ordinary.count, 1);
        assert_eq!(ordinary.events[0].event_id, MacroEventId(20));

        let mut tokened = MacroEventScheduler::new();
        tokened.admit_group(&shot).unwrap();
        let crack = tokened.reserve_next_activation(200);
        assert_eq!(
            tokened.commit_reserved(crack, 99),
            Err(EventReservationError::FrameMismatch)
        );
        assert_eq!(tokened.queued_len(), 2);
        let committed = tokened.commit_reserved(crack, 100).unwrap();
        assert_eq!(committed.count, 1);
        assert_eq!(committed.events[0].event_id, MacroEventId(20));
        assert_eq!(tokened.queued_len(), 1);
        let blast = tokened.reserve_next_activation(200);
        assert_eq!(blast.count, 1);
        assert_eq!(blast.activation_frame(), Some(150));
        assert_eq!(blast.events[0].event_id, MacroEventId(21));
    }

    #[test]
    fn crack_and_blast_admit_atomically_and_activate_in_dedicated_roles() {
        let mut scheduler = MacroEventScheduler::new();
        let shot = [
            event(20, 7, EventRole::BallisticCrack, 100),
            event(21, 7, EventRole::BallisticBlast, 150),
        ];
        scheduler.admit_group(&shot).unwrap();
        assert_eq!(scheduler.queued_len(), 2);
        let crack = scheduler.activate_due(100);
        assert_eq!(crack.count, 1);
        assert_eq!(crack.events[0].event_id, MacroEventId(20));
        assert_eq!(
            scheduler.active_event(EventRole::BallisticCrack),
            Some(MacroEventId(20))
        );
        let blast = scheduler.activate_due(150);
        assert_eq!(blast.count, 1);
        assert_eq!(blast.events[0].event_id, MacroEventId(21));
        assert_eq!(
            scheduler.active_event(EventRole::BallisticBlast),
            Some(MacroEventId(21))
        );
    }

    #[test]
    fn failed_group_admission_leaves_the_queue_unchanged() {
        let mut queue = MacroEventQueue::new();
        let invalid = [
            event(1, 10, EventRole::BallisticCrack, 10),
            event(2, 11, EventRole::BallisticBlast, 20),
        ];
        assert_eq!(
            queue.admit_group(&invalid),
            Err(EventAdmissionError::MixedGroupIdentity)
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn dormant_queue_meets_the_quarter_mebibyte_budget() {
        let queue = MacroEventQueue::new();
        let bytes = queue.persistent_bytes();
        println!(
            "MACRO_QUEUE_MEMORY bytes={bytes} kib={:.3}",
            bytes as f64 / 1024.0
        );
        assert!(bytes <= 256 * 1024, "queue uses {bytes} bytes");
    }

    #[test]
    fn all_1024_slots_admit_without_decoded_audio_or_delay_history() {
        let mut queue = MacroEventQueue::new();
        for index in 0..MACRO_EVENT_QUEUE_CAPACITY {
            queue
                .admit_group(&[event(
                    index as u64 + 1,
                    index as u64 + 1,
                    EventRole::StandardImpulse,
                    index as u64,
                )])
                .unwrap();
        }
        assert_eq!(queue.len(), MACRO_EVENT_QUEUE_CAPACITY);
        assert_eq!(
            queue.admit_group(&[event(2_000, 2_000, EventRole::StandardImpulse, 2_000,)]),
            Err(EventAdmissionError::QueueFull)
        );
    }

    /// Regression: a later-frame event with a smaller identity key must never
    /// displace an earlier-frame candidate for the same role. The optimized
    /// single-pass scan once ordered candidates by `(atomic_group_id, event_id)`
    /// alone, so event 1 (frame 20) replaced event 100 (frame 10) and the
    /// subsequent `activation_frame == target_frame` filter rejected it,
    /// reserving nothing despite a ready event.
    #[test]
    fn earlier_frame_wins_over_smaller_identity_key_within_a_role() {
        let mut scheduler = MacroEventScheduler::new();
        scheduler
            .admit_group(&[event(100, 100, EventRole::StandardImpulse, 10)])
            .unwrap();
        scheduler
            .admit_group(&[event(1, 1, EventRole::StandardImpulse, 20)])
            .unwrap();

        // Lookahead covers both frames; the earliest frame must win even
        // though the later event carries the smaller identity key.
        let reservation = scheduler.reserve_next_activation(20);
        assert_eq!(reservation.count, 1);
        assert_eq!(reservation.activation_frame(), Some(10));
        assert_eq!(reservation.events[0].event_id, MacroEventId(100));
        assert_eq!(scheduler.validate_reserved(reservation), Ok(()));
    }
}
