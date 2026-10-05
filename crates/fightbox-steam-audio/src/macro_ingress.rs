//! Steam-facing consumption of listener-local macro ingress events.
//!
//! Macro distance and air conditioning are applied to decoded program exactly
//! once here. Detailed events then enter a reserved Steam source slot at the
//! ingress proxy, so the existing graph owns only [`FinalLocalLeg`]. Fallback
//! events apply that final leg locally and optionally feed the one shared
//! diffuse field.

use fightbox_api::diffuse::DiffuseFieldProfile;
use fightbox_api::macro_transport::{EventPropagationEligibility, EventRole, MacroEventId};
use fightbox_api::spectral::{SpectralStage, SpectralTransfer, SpectralTransferError};
use fightbox_api::{EnuVector3, Pose};
use fightbox_runtime::backend::{
    MAX_ACTIVE_SOURCES, MAX_SPATIAL_PRESENTATION_FEEDS, SimulationUpdate, SourceMotion,
    SpatialAmbisonicChannelOrder, SpatialAmbisonicNormalization, SpatialAmbisonicOrder,
    SpatialBackendRenderError, SpatialBackendRenderGraph, SpatialBackendSourceBlock,
    SpatialEnvironmentalBasis, SpatialFeedPlacement, SpatialPresentationComponent,
    SpatialPresentationFeedMetadata, SpatialPropagationRenderBlock, SpatialTailRetirementState,
};
use fightbox_runtime::{
    EventReleaseError, FinalLocalLeg, IngressEventFamily, IngressRenderAuthority,
    LocalCellAuthority, LocalIngressActivation, LocalIngressActivationBatch, MacroLocalIngress,
    SharedDiffuseError, SharedDiffuseField, SpectralFilterError, SpectralTransferFilter,
};
use fightbox_runtime::{SnapshotPublication, SnapshotReader, SnapshotWriter};

use crate::{MultiSourceDescriptor, SourcePriorityClass};

/// Canonical high slots reserved for the four retained event roles.
pub const DEFAULT_MACRO_EVENT_SOURCE_INDICES: [usize; EventRole::COUNT] = [12, 13, 14, 15];
const MAX_MACRO_ENVIRONMENTAL_ALIGNMENT_FRAMES: usize = 16_384;
const MAX_MACRO_ECHO_EXCESS_SECONDS: f32 = 1.2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacroIngressSlotMap {
    source_indices: [usize; EventRole::COUNT],
}

impl MacroIngressSlotMap {
    pub fn new(source_indices: [usize; EventRole::COUNT]) -> Result<Self, SteamMacroIngressError> {
        for (index, source_index) in source_indices.iter().copied().enumerate() {
            if source_index >= MAX_ACTIVE_SOURCES {
                return Err(SteamMacroIngressError::InvalidSourceIndex);
            }
            if source_indices[..index].contains(&source_index) {
                return Err(SteamMacroIngressError::DuplicateSourceIndex);
            }
        }
        Ok(Self { source_indices })
    }

    #[must_use]
    pub const fn source_index(self, role: EventRole) -> usize {
        self.source_indices[role.index()]
    }

    /// Construction-time descriptor for one stable role slot.
    #[must_use]
    pub fn descriptor(
        self,
        role: EventRole,
        initial_position_enu: EnuVector3,
    ) -> MultiSourceDescriptor {
        let eligibility = role.local_propagation_eligibility();
        MultiSourceDescriptor::at(initial_position_enu)
            .with_initially_active(false)
            .with_source_priority(SourcePriorityClass::TransientEvent)
            .with_pathing_send(eligibility.baked_reflections)
            .with_reflection_send(eligibility.baked_reflections)
    }
}

impl Default for MacroIngressSlotMap {
    fn default() -> Self {
        Self {
            source_indices: DEFAULT_MACRO_EVENT_SOURCE_INDICES,
        }
    }
}

pub const MAX_MACRO_ECHO_TAPS: usize = 4;
const ECHO_SPECTRAL_CROSSOVER_FREQUENCIES_HZ: [f32; 7] = [
    176.776_7,
    353.553_4,
    707.106_8,
    1_414.213_6,
    2_828.427_2,
    5_656.854_5,
    11_313.709,
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum EchoSpectralPressureMode {
    Flat(f32),
    Shaped([f32; fightbox_api::spectral::SPECTRAL_BAND_COUNT]),
}

/// Signed pressure-domain counterpart to Runtime's magnitude-only transfer
/// filter. Authored reflection coefficients may invert polarity independently
/// by band, so converting them to dB magnitudes would destroy package truth.
#[derive(Clone, Debug)]
struct EchoSpectralPressureFilter {
    mode: EchoSpectralPressureMode,
    crossover_alpha: [f32; 7],
    lowpass_state: [f32; 7],
    shaped_state_ready: bool,
}

impl EchoSpectralPressureFilter {
    fn new(sample_rate_hz: u32) -> Option<Self> {
        if sample_rate_hz == 0 {
            return None;
        }
        let sample_rate_hz = sample_rate_hz as f32;
        Some(Self {
            mode: EchoSpectralPressureMode::Flat(0.0),
            crossover_alpha: ECHO_SPECTRAL_CROSSOVER_FREQUENCIES_HZ.map(|frequency_hz| {
                (1.0 - (-core::f32::consts::TAU * frequency_hz / sample_rate_hz).exp())
                    .clamp(0.0, 1.0)
            }),
            lowpass_state: [0.0; 7],
            shaped_state_ready: false,
        })
    }

    fn reset(&mut self) {
        self.lowpass_state = [0.0; 7];
        self.shaped_state_ready = false;
    }

    fn set_pressure_gains(
        &mut self,
        gains: [f32; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
    ) -> bool {
        if gains.iter().any(|gain| !gain.is_finite()) {
            return false;
        }
        self.mode = if gains[1..]
            .iter()
            .all(|gain| gain.to_bits() == gains[0].to_bits())
        {
            EchoSpectralPressureMode::Flat(gains[0])
        } else {
            EchoSpectralPressureMode::Shaped(gains)
        };
        true
    }

    #[inline]
    fn process_sample(&mut self, input: f32) -> f32 {
        let gains = match self.mode {
            EchoSpectralPressureMode::Flat(gain) => return input * gain,
            EchoSpectralPressureMode::Shaped(gains) => gains,
        };
        if !self.shaped_state_ready {
            self.lowpass_state = [input; 7];
            self.shaped_state_ready = true;
        }
        for (state, alpha) in self
            .lowpass_state
            .iter_mut()
            .zip(self.crossover_alpha.iter().copied())
        {
            *state += alpha * (input - *state);
        }
        let mut output = self.lowpass_state[0] * gains[0];
        for band_index in 1..7 {
            output += (self.lowpass_state[band_index] - self.lowpass_state[band_index - 1])
                * gains[band_index];
        }
        output + (input - self.lowpass_state[6]) * gains[7]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SteamMacroEchoTap {
    pub active: bool,
    pub delay_samples: f32,
    /// Sound-propagation arrival vector in world ENU; the ACN seam converts
    /// this to an apparent reflector direction exactly once.
    pub arrival_direction_enu: EnuVector3,
    pub spectral_pressure_gain: [f32; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
    pub path_key: [u8; 16],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SteamMacroEchoPlan {
    pub tap_count: u8,
    pub taps: [SteamMacroEchoTap; MAX_MACRO_ECHO_TAPS],
}

impl SteamMacroEchoPlan {
    pub const OFF: Self = Self {
        tap_count: 0,
        taps: [SteamMacroEchoTap {
            active: false,
            delay_samples: 0.0,
            arrival_direction_enu: EnuVector3::new(0.0, 0.0, 0.0),
            spectral_pressure_gain: [0.0; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
            path_key: [0; 16],
        }; MAX_MACRO_ECHO_TAPS],
    };

    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.tap_count != 0
    }

    #[must_use]
    pub fn maximum_delay_frames(self) -> u64 {
        self.taps[..usize::from(self.tap_count)]
            .iter()
            .map(|tap| tap.delay_samples.ceil().max(0.0) as u64)
            .max()
            .unwrap_or(0)
    }
}

impl Default for SteamMacroEchoPlan {
    fn default() -> Self {
        Self::OFF
    }
}

/// Fixed-size render command copied from one control-owned activation.
///
/// No authority `Arc`, package identity `String`, or other reclaimable resource
/// crosses this boundary. Control retains those objects until a later render
/// acknowledgement makes reclamation safe off the audio thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SteamMacroIngressCommand {
    pub activation_epoch: u64,
    pub direct_generation: u64,
    pub effective_frame: u64,
    pub event_id: MacroEventId,
    pub family: IngressEventFamily,
    pub role: EventRole,
    pub asset_key: u64,
    /// Even Swift canonical-provider discontinuity generation proving that the
    /// exact seek is resident before this command becomes audible.
    pub asset_readiness_generation: u64,
    pub program_seek_frame: u64,
    pub program_start_frame: u64,
    /// End of canonical provider PCM; later tail retention stays active but
    /// requests silence rather than reading past the prepared asset interval.
    pub program_end_frame: u64,
    pub tail_deadline_frame: u64,
    pub echo_plan: SteamMacroEchoPlan,
    pub authority_world_generation: u64,
    pub macro_distance_gain: f32,
    pub macro_atmosphere_gain_db: [f32; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
    pub local_leg: FinalLocalLeg,
    pub eligibility: EventPropagationEligibility,
    pub mode: SteamMacroIngressMode,
}

impl Default for SteamMacroIngressCommand {
    fn default() -> Self {
        Self {
            activation_epoch: 0,
            direct_generation: 0,
            effective_frame: 0,
            event_id: MacroEventId(0),
            family: IngressEventFamily::General { atomic_group_id: 0 },
            role: EventRole::CinematicImpulse,
            asset_key: 0,
            asset_readiness_generation: 0,
            program_seek_frame: 0,
            program_start_frame: 0,
            program_end_frame: 0,
            tail_deadline_frame: 0,
            echo_plan: SteamMacroEchoPlan::OFF,
            authority_world_generation: 0,
            macro_distance_gain: 0.0,
            macro_atmosphere_gain_db: [0.0; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
            local_leg: FinalLocalLeg {
                ingress_proxy_enu: EnuVector3::default(),
                remote_direction_enu: EnuVector3::default(),
                distance_m: 0.0,
                delay_frames: 0,
                distance_gain: 0.0,
                atmosphere_gain_db: [0.0; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
            },
            eligibility: EventPropagationEligibility {
                detailed_direct: false,
                statistical_ground: false,
                baked_reflections: false,
                authored_echo: false,
                shared_diffuse: false,
            },
            mode: SteamMacroIngressMode::DetailedLocal,
        }
    }
}

impl SteamMacroIngressCommand {
    fn from_activation(
        activation: &LocalIngressActivation,
        activation_epoch: u64,
        direct_generation: u64,
        effective_frame: u64,
    ) -> Self {
        let (mode, authority_world_generation, program_start_frame) =
            match &activation.render_authority {
                IngressRenderAuthority::DetailedLocal { authority } => (
                    SteamMacroIngressMode::DetailedLocal,
                    authority.world_generation,
                    activation.macro_conditioning.arrival_frame(),
                ),
                IngressRenderAuthority::MacroFallback {
                    observed_authority,
                    shared_diffuse,
                    ..
                } => (
                    SteamMacroIngressMode::MacroFallback {
                        shared_diffuse: *shared_diffuse,
                    },
                    observed_authority
                        .as_ref()
                        .map_or(0, |authority| authority.world_generation),
                    activation
                        .macro_conditioning
                        .arrival_frame()
                        .saturating_add(activation.local_leg.delay_frames),
                ),
            };
        Self {
            activation_epoch,
            direct_generation,
            effective_frame,
            event_id: activation.event_id,
            family: activation.family,
            role: activation.role,
            asset_key: activation.asset_key,
            asset_readiness_generation: 0,
            program_seek_frame: activation.program_seek_frame,
            program_start_frame,
            program_end_frame: activation.tail_deadline_frame,
            tail_deadline_frame: activation.tail_deadline_frame,
            echo_plan: SteamMacroEchoPlan::OFF,
            authority_world_generation,
            macro_distance_gain: activation.macro_conditioning.distance_gain(),
            macro_atmosphere_gain_db: *activation.macro_conditioning.atmosphere_gain_db(),
            local_leg: activation.local_leg,
            eligibility: activation.eligibility,
            mode,
        }
    }

    /// Contributes macro transport (and, for fallback only, the final local
    /// leg) to the one Runtime atmosphere stage. Other named stage stems remain
    /// unchanged; the caller publishes the returned transfer rather than
    /// installing a second macro filter in the backend wrapper.
    pub fn compose_runtime_spectral_transfer(
        self,
        mut base: SpectralTransfer,
    ) -> Result<SpectralTransfer, SpectralTransferError> {
        let base_atmosphere = base.stage_gain_db(SpectralStage::Atmosphere);
        let mut composed_atmosphere = [0.0; fightbox_api::spectral::SPECTRAL_BAND_COUNT];
        for (band_index, composed) in composed_atmosphere.iter_mut().enumerate() {
            let mut gain_db = f64::from(base_atmosphere[band_index])
                + f64::from(self.macro_atmosphere_gain_db[band_index]);
            if matches!(self.mode, SteamMacroIngressMode::MacroFallback { .. }) {
                gain_db += f64::from(self.local_leg.atmosphere_gain_db[band_index]);
            }
            *composed = gain_db as f32;
        }
        base.set_stage(SpectralStage::Atmosphere, composed_atmosphere)?;
        Ok(base)
    }

    /// Scalar distance term remaining after Runtime's source drive/safety.
    /// Steam owns the final local leg for detailed routing; fallback owns it
    /// here because that route is deliberately excluded from Steam.
    #[must_use]
    pub fn runtime_distance_gain(self) -> f32 {
        match self.mode {
            SteamMacroIngressMode::DetailedLocal => self.macro_distance_gain,
            SteamMacroIngressMode::MacroFallback { .. } => {
                self.macro_distance_gain * self.local_leg.distance_gain
            }
        }
    }

    /// Backend source state for the detailed route. Fallback deliberately has
    /// no Steam-active source; its Runtime-active program is owned by the
    /// outer neutral wrapper.
    #[must_use]
    pub fn detailed_source_motion(self) -> Option<SourceMotion> {
        if !matches!(self.mode, SteamMacroIngressMode::DetailedLocal) {
            return None;
        }
        let direction = self.local_leg.remote_direction_enu;
        let up = if direction.up_m.abs() > 0.99 {
            EnuVector3::new(0.0, 1.0, 0.0)
        } else {
            EnuVector3::new(0.0, 0.0, 1.0)
        };
        Some(SourceMotion {
            active: true,
            pose: fightbox_api::Pose {
                position: self.local_leg.ingress_proxy_enu,
                forward: direction,
                up,
            },
            linear_velocity_mps: EnuVector3::default(),
        })
    }
}

/// One correlated, at-most-four-role command publication.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SteamMacroIngressCommandBatch {
    count: u8,
    commands: [SteamMacroIngressCommand; EventRole::COUNT],
}

impl SteamMacroIngressCommandBatch {
    #[must_use]
    pub fn from_activations(
        activations: &LocalIngressActivationBatch,
        activation_epoch: u64,
        direct_generation: u64,
        effective_frame: u64,
    ) -> Self {
        let mut output = Self::default();
        for (index, activation) in activations.iter().enumerate() {
            output.commands[index] = SteamMacroIngressCommand::from_activation(
                activation,
                activation_epoch,
                direct_generation,
                effective_frame,
            );
            output.count += 1;
        }
        output
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &SteamMacroIngressCommand> {
        self.commands[..self.len()].iter()
    }

    #[must_use]
    pub fn for_role(&self, role: EventRole) -> Option<&SteamMacroIngressCommand> {
        self.iter().find(|command| command.role == role)
    }

    /// Binds one already-validated canonical-provider readiness generation
    /// before publication. Zero and odd generations are never callback-ready.
    pub fn bind_asset_readiness(&mut self, role: EventRole, generation: u64) -> bool {
        if generation == 0 || generation & 1 != 0 {
            return false;
        }
        let len = self.len();
        let Some(command) = self.commands[..len]
            .iter_mut()
            .find(|command| command.role == role)
        else {
            return false;
        };
        command.asset_readiness_generation = generation;
        true
    }

    /// Binds the dormant scheduler record's original provider end, which is
    /// earlier than a detailed route's control-owned propagation/tail pin.
    pub fn bind_program_end_frame(&mut self, role: EventRole, program_end_frame: u64) -> bool {
        let len = self.len();
        let Some(command) = self.commands[..len]
            .iter_mut()
            .find(|command| command.role == role)
        else {
            return false;
        };
        if program_end_frame <= command.program_start_frame
            || program_end_frame > command.tail_deadline_frame
        {
            return false;
        }
        command.program_end_frame = program_end_frame;
        true
    }

    pub fn bind_echo_plan(
        &mut self,
        role: EventRole,
        plan: SteamMacroEchoPlan,
        tail_deadline_frame: u64,
    ) -> bool {
        let len = self.len();
        let Some(command) = self.commands[..len]
            .iter_mut()
            .find(|command| command.role == role)
        else {
            return false;
        };
        if usize::from(plan.tap_count) > MAX_MACRO_ECHO_TAPS
            || tail_deadline_frame < command.tail_deadline_frame
        {
            return false;
        }
        command.echo_plan = plan;
        command.tail_deadline_frame = tail_deadline_frame;
        true
    }

    #[must_use]
    pub fn is_asset_ready(&self) -> bool {
        !self.is_empty()
            && self.iter().all(|command| {
                command.asset_readiness_generation != 0
                    && command.asset_readiness_generation & 1 == 0
                    && command.program_end_frame > command.program_start_frame
                    && command.program_end_frame <= command.tail_deadline_frame
            })
    }
}

impl Default for SteamMacroIngressCommandBatch {
    fn default() -> Self {
        Self {
            count: 0,
            commands: [SteamMacroIngressCommand::default(); EventRole::COUNT],
        }
    }
}

/// Unique control-thread endpoint for the bounded command handoff.
pub struct SteamMacroIngressPublisher {
    inner: crate::world_swap::Producer<SteamMacroIngressCommandBatch>,
}

impl SteamMacroIngressPublisher {
    /// Whether the audio thread has consumed the previous batch.
    ///
    /// A false result is backpressure: the queue owner must leave newly due
    /// events dormant and retry instead of consuming or overwriting them.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    /// Claims mailbox capacity before the control transaction runs direct.
    /// No command becomes visible until `publish_reserved`; direct failure may
    /// return the empty claim with `discard_reservation`.
    pub fn try_reserve(&mut self) -> bool {
        self.inner.try_reserve()
    }

    pub fn publish_reserved(
        &mut self,
        batch: SteamMacroIngressCommandBatch,
    ) -> Result<(), SteamMacroIngressCommandBatch> {
        if !batch.is_asset_ready() {
            return Err(batch);
        }
        self.inner.publish_reserved(batch)
    }

    pub fn discard_reservation(&mut self) -> bool {
        self.inner.discard_reservation()
    }

    /// Copies no owned authority into the callback. The batch is returned
    /// intact when the single preallocated slot is full.
    pub fn try_publish(
        &mut self,
        batch: SteamMacroIngressCommandBatch,
    ) -> Result<(), SteamMacroIngressCommandBatch> {
        if !batch.is_asset_ready() {
            return Err(batch);
        }
        self.inner.try_push(batch)
    }
}

/// Unique audio-thread endpoint for the bounded command handoff.
pub struct SteamMacroIngressReceiver {
    inner: crate::world_swap::Consumer<SteamMacroIngressCommandBatch>,
}

impl SteamMacroIngressReceiver {
    /// Nonblocking callback-boundary take. Empty or contested publication is
    /// retried on the next block and never waits on the control thread.
    pub fn try_take(&mut self) -> Option<SteamMacroIngressCommandBatch> {
        self.inner.try_pop()
    }
}

/// Creates the one-slot, allocation-free-after-construction ownership handoff
/// used to copy correlated activation commands into the audio callback.
#[must_use]
pub fn steam_macro_ingress_activation_channel()
-> (SteamMacroIngressPublisher, SteamMacroIngressReceiver) {
    let (publisher, receiver) = crate::world_swap::channel();
    (
        SteamMacroIngressPublisher { inner: publisher },
        SteamMacroIngressReceiver { inner: receiver },
    )
}

/// Audio-to-control lifecycle observation. All fields are compact copies; the
/// callback never transfers or releases control-owned authority/resources.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SteamMacroIngressAcknowledgementKind {
    #[default]
    Activated,
    Deactivated,
    /// Permanent missed/stale rejection. A retryable render/provider discard
    /// emits no acknowledgement and leaves cursor/role state untouched.
    TerminalRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteamMacroIngressAcknowledgement {
    pub kind: SteamMacroIngressAcknowledgementKind,
    pub activation_epoch: u64,
    pub direct_generation: u64,
    pub audio_frame: u64,
    pub event_id: MacroEventId,
    pub role: EventRole,
    pub asset_key: u64,
    pub asset_readiness_generation: u64,
}

impl Default for SteamMacroIngressAcknowledgement {
    fn default() -> Self {
        Self {
            kind: SteamMacroIngressAcknowledgementKind::Activated,
            activation_epoch: 0,
            direct_generation: 0,
            audio_frame: 0,
            event_id: MacroEventId(0),
            role: EventRole::CinematicImpulse,
            asset_key: 0,
            asset_readiness_generation: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteamMacroIngressAcknowledgementBatch {
    count: u8,
    acknowledgements: [SteamMacroIngressAcknowledgement; EventRole::COUNT],
}

impl SteamMacroIngressAcknowledgementBatch {
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &SteamMacroIngressAcknowledgement> {
        self.acknowledgements[..self.len()].iter()
    }

    /// Appends one role-qualified acknowledgement without overwriting an
    /// earlier observation. A false result is retained callback backpressure.
    pub fn try_push(&mut self, acknowledgement: SteamMacroIngressAcknowledgement) -> bool {
        if self.len() == EventRole::COUNT
            || self
                .iter()
                .any(|existing| existing.role == acknowledgement.role)
        {
            return false;
        }
        let index = self.len();
        self.acknowledgements[index] = acknowledgement;
        self.count += 1;
        true
    }
}

impl Default for SteamMacroIngressAcknowledgementBatch {
    fn default() -> Self {
        Self {
            count: 0,
            acknowledgements: [SteamMacroIngressAcknowledgement::default(); EventRole::COUNT],
        }
    }
}

/// Unique audio-thread endpoint for the bounded acknowledgement handoff.
pub struct SteamMacroIngressAcknowledgementPublisher {
    inner: crate::world_swap::Producer<SteamMacroIngressAcknowledgementBatch>,
}

impl SteamMacroIngressAcknowledgementPublisher {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }

    pub fn try_publish(
        &mut self,
        batch: SteamMacroIngressAcknowledgementBatch,
    ) -> Result<(), SteamMacroIngressAcknowledgementBatch> {
        self.inner.try_push(batch)
    }
}

/// Unique control-thread endpoint for acknowledged activation/deactivation.
pub struct SteamMacroIngressAcknowledgementReceiver {
    inner: crate::world_swap::Consumer<SteamMacroIngressAcknowledgementBatch>,
}

impl SteamMacroIngressAcknowledgementReceiver {
    pub fn try_take(&mut self) -> Option<SteamMacroIngressAcknowledgementBatch> {
        self.inner.try_pop()
    }
}

#[must_use]
pub fn steam_macro_ingress_acknowledgement_channel() -> (
    SteamMacroIngressAcknowledgementPublisher,
    SteamMacroIngressAcknowledgementReceiver,
) {
    let (publisher, receiver) = crate::world_swap::channel();
    (
        SteamMacroIngressAcknowledgementPublisher { inner: publisher },
        SteamMacroIngressAcknowledgementReceiver { inner: receiver },
    )
}

/// Exact initialized one-slot payloads, excluding Arc control blocks and
/// allocator metadata, for persistent binding telemetry.
#[must_use]
pub const fn steam_macro_ingress_command_channel_payload_bytes() -> u64 {
    crate::world_swap::shared_payload_bytes::<SteamMacroIngressCommandBatch>()
}

#[must_use]
pub const fn steam_macro_ingress_acknowledgement_channel_payload_bytes() -> u64 {
    crate::world_swap::shared_payload_bytes::<SteamMacroIngressAcknowledgementBatch>()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteamMacroIngressMode {
    DetailedLocal,
    MacroFallback { shared_diffuse: bool },
}

/// Control-owned role overlay applied to every complete host publication.
/// It separates Runtime activity from Steam activity so fallback cannot acquire
/// a hidden Steam path while its provider plane remains callback-required.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacroIngressControlOverlay {
    slots: MacroIngressSlotMap,
    active: [Option<SteamMacroIngressCommand>; EventRole::COUNT],
}

impl MacroIngressControlOverlay {
    #[must_use]
    pub const fn new(slots: MacroIngressSlotMap) -> Self {
        Self {
            slots,
            active: [None; EventRole::COUNT],
        }
    }

    /// Installs a whole command batch after validating every role. Replaying
    /// the exact token is idempotent; a successor cannot overtake its role's
    /// deactivation acknowledgement.
    pub fn activate(
        &mut self,
        batch: SteamMacroIngressCommandBatch,
    ) -> Result<(), SteamMacroIngressError> {
        if !batch.is_asset_ready() {
            return Err(SteamMacroIngressError::AssetNotReady);
        }
        for command in batch.iter() {
            if command.direct_generation == 0
                || command.effective_frame > command.tail_deadline_frame
            {
                return Err(SteamMacroIngressError::InvalidTimeline);
            }
            if let Some(active) = self.active[command.role.index()]
                && (active.activation_epoch != command.activation_epoch
                    || active.event_id != command.event_id
                    || active.asset_readiness_generation != command.asset_readiness_generation)
            {
                return Err(SteamMacroIngressError::RoleAlreadyActive);
            }
        }
        for command in batch.iter().copied() {
            self.active[command.role.index()] = Some(command);
        }
        Ok(())
    }

    /// Validates the freshly decoded complete host frame before any engine
    /// overlay. Granular setters reject these indices independently.
    pub fn validate_complete_host_update(
        &self,
        update: &SimulationUpdate,
    ) -> Result<(), SteamMacroIngressError> {
        for role in EventRole::ALL {
            if update.sources[self.slots.source_index(role)].active {
                return Err(SteamMacroIngressError::ReservedSourceOwnedByHost);
            }
        }
        Ok(())
    }

    /// Overwrites engine-owned slots on every backend publication, including
    /// listener-only updates whose retained update already contains a previous
    /// overlay. Detailed routes enter Steam; fallback routes remain inactive.
    pub fn overlay_backend_update(&self, update: &mut SimulationUpdate) {
        for role in EventRole::ALL {
            let source_index = self.slots.source_index(role);
            update.sources[source_index].active = false;
            if let Some(command) = self.active[role.index()]
                && let Some(motion) = command.detailed_source_motion()
            {
                update.sources[source_index] = motion;
            }
        }
    }

    /// Returns the correlated Runtime active set after overlay. Detailed and
    /// fallback programs are both required by Runtime; only detailed is active
    /// in the separate Steam update above.
    #[must_use]
    pub fn overlay_runtime_activity(
        &self,
        mut host_activity: [bool; MAX_ACTIVE_SOURCES],
    ) -> [bool; MAX_ACTIVE_SOURCES] {
        for role in EventRole::ALL {
            let source_index = self.slots.source_index(role);
            host_activity[source_index] = self.active[role.index()].is_some();
        }
        host_activity
    }

    #[must_use]
    pub const fn active_command(&self, role: EventRole) -> Option<SteamMacroIngressCommand> {
        self.active[role.index()]
    }

    /// Applies only a token-exact terminal audio acknowledgement. The returned
    /// copy lets control release the matching ingress/resource pin off audio.
    pub fn acknowledge(
        &mut self,
        acknowledgement: SteamMacroIngressAcknowledgement,
    ) -> Option<SteamMacroIngressCommand> {
        if acknowledgement.kind == SteamMacroIngressAcknowledgementKind::Activated {
            return None;
        }
        let active = self.active[acknowledgement.role.index()]?;
        if active.activation_epoch != acknowledgement.activation_epoch
            || active.direct_generation != acknowledgement.direct_generation
            || active.event_id != acknowledgement.event_id
            || active.asset_key != acknowledgement.asset_key
            || active.asset_readiness_generation != acknowledgement.asset_readiness_generation
        {
            return None;
        }
        self.active[acknowledgement.role.index()] = None;
        Some(active)
    }
}

impl Default for MacroIngressControlOverlay {
    fn default() -> Self {
        Self::new(MacroIngressSlotMap::default())
    }
}

/// Audio-owned fixed route publication consumed by the spatial-backend
/// wrapper. Commands are scalar copies; clearing a role never releases control
/// authority or Swift resources.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MacroIngressRenderSnapshot {
    active: [Option<SteamMacroIngressCommand>; EventRole::COUNT],
}

impl MacroIngressRenderSnapshot {
    #[must_use]
    pub const fn active_command(&self, role: EventRole) -> Option<SteamMacroIngressCommand> {
        self.active[role.index()]
    }

    pub fn set_active(&mut self, command: SteamMacroIngressCommand) {
        self.active[command.role.index()] = Some(command);
    }

    pub fn clear(&mut self, role: EventRole) {
        self.active[role.index()] = None;
    }
}

/// Creates the fixed audio-controller to backend-wrapper publication.
#[must_use]
pub fn macro_ingress_render_snapshot_channel() -> (
    SnapshotWriter<MacroIngressRenderSnapshot>,
    SnapshotReader<MacroIngressRenderSnapshot>,
) {
    SnapshotPublication::new(MacroIngressRenderSnapshot::default())
}

/// Audio-owned wrapper which keeps fallback programs out of Steam while
/// applying the one route-exact distance scalar after Runtime conditioning.
/// Detailed programs continue through the existing Steam graph; point
/// fallback programs enter the reserved center presentation plane with
/// world-space direction metadata. Shared fallback additionally feeds one
/// decorrelated scalar field which is published only as world-unrotated N3D
/// ACN0; the legacy stereo field never crosses the neutral bank seam.
fn n3d_acn_order2_from_arrival(direction: EnuVector3) -> Option<[f32; 9]> {
    let magnitude = (direction.east_m * direction.east_m
        + direction.north_m * direction.north_m
        + direction.up_m * direction.up_m)
        .sqrt();
    if !magnitude.is_finite() || magnitude <= f32::EPSILON {
        return None;
    }
    // Authority stores sound-propagation arrival direction. Ambisonic source
    // direction points back toward the final interaction.
    let x = -direction.east_m / magnitude;
    let y = -direction.north_m / magnitude;
    let z = -direction.up_m / magnitude;
    let sqrt3 = 3.0_f32.sqrt();
    let sqrt5 = 5.0_f32.sqrt();
    let sqrt15 = 15.0_f32.sqrt();
    Some([
        1.0,
        sqrt3 * y,
        sqrt3 * z,
        sqrt3 * x,
        sqrt15 * x * y,
        sqrt15 * y * z,
        0.5 * sqrt5 * (3.0 * z * z - 1.0),
        sqrt15 * x * z,
        0.5 * sqrt15 * (x * x - y * y),
    ])
}

pub struct MacroIngressSpatialRenderGraph {
    inner: Box<dyn SpatialBackendRenderGraph>,
    routes: SnapshotReader<MacroIngressRenderSnapshot>,
    slots: MacroIngressSlotMap,
    sample_rate_hz: u32,
    block_size_frames: usize,
    scaled_programs: [Vec<f32>; EventRole::COUNT],
    diffuse_send: Vec<f32>,
    diffuse_left: Vec<f32>,
    diffuse_right: Vec<f32>,
    shared_diffuse: SharedDiffuseField,
    environmental_alignment: Vec<f32>,
    environmental_alignment_head: usize,
    environmental_latency_frames: usize,
    echo_history: [Vec<f32>; EventRole::COUNT],
    echo_history_head: [usize; EventRole::COUNT],
    echo_filters: [[EchoSpectralPressureFilter; MAX_MACRO_ECHO_TAPS]; EventRole::COUNT],
    echo_coefficients: [[[f32; 9]; MAX_MACRO_ECHO_TAPS]; EventRole::COUNT],
    echo_activation_epoch: [u64; EventRole::COUNT],
    echo_tail_frames_remaining: [usize; EventRole::COUNT],
    diffuse_tail_capacity_frames: usize,
    diffuse_tail_frames_remaining: usize,
    tail_retiring: bool,
}

impl MacroIngressSpatialRenderGraph {
    pub fn new(
        inner: Box<dyn SpatialBackendRenderGraph>,
        routes: SnapshotReader<MacroIngressRenderSnapshot>,
        slots: MacroIngressSlotMap,
        sample_rate_hz: u32,
        block_size_frames: usize,
        diffuse_profile: DiffuseFieldProfile,
    ) -> Result<Self, SteamMacroIngressError> {
        if block_size_frames == 0 {
            return Err(SteamMacroIngressError::InvalidBlockSize);
        }
        let shared_diffuse = SharedDiffuseField::new(sample_rate_hz, diffuse_profile)
            .map_err(SteamMacroIngressError::SharedDiffuse)?;
        let diffuse_tail_capacity_frames =
            (f64::from(sample_rate_hz) * f64::from(diffuse_profile.rt60_s) * 2.0)
                .ceil()
                .max(1.0) as usize;
        let echo_history_frames = (sample_rate_hz as f32 * MAX_MACRO_ECHO_EXCESS_SECONDS).ceil()
            as usize
            + MAX_MACRO_ENVIRONMENTAL_ALIGNMENT_FRAMES
            + 2;
        let echo_filters = std::array::from_fn(|_| {
            std::array::from_fn(|_| {
                EchoSpectralPressureFilter::new(sample_rate_hz)
                    .expect("validated macro sample rate constructs fixed echo filters")
            })
        });
        Ok(Self {
            inner,
            routes,
            slots,
            sample_rate_hz,
            block_size_frames,
            scaled_programs: std::array::from_fn(|_| vec![0.0; block_size_frames]),
            diffuse_send: vec![0.0; block_size_frames],
            diffuse_left: vec![0.0; block_size_frames],
            diffuse_right: vec![0.0; block_size_frames],
            shared_diffuse,
            environmental_alignment: vec![0.0; MAX_MACRO_ENVIRONMENTAL_ALIGNMENT_FRAMES + 1],
            environmental_alignment_head: 0,
            environmental_latency_frames: 0,
            echo_history: std::array::from_fn(|_| vec![0.0; echo_history_frames]),
            echo_history_head: [0; EventRole::COUNT],
            echo_filters,
            echo_coefficients: [[[0.0; 9]; MAX_MACRO_ECHO_TAPS]; EventRole::COUNT],
            echo_activation_epoch: [0; EventRole::COUNT],
            echo_tail_frames_remaining: [0; EventRole::COUNT],
            diffuse_tail_capacity_frames,
            diffuse_tail_frames_remaining: 0,
            tail_retiring: false,
        })
    }

    #[must_use]
    pub fn persistent_payload_bytes(&self) -> u64 {
        self.scaled_programs
            .iter()
            .map(|plane| plane.capacity() as u64 * core::mem::size_of::<f32>() as u64)
            .sum::<u64>()
            .saturating_add(
                (self.diffuse_send.capacity() as u64
                    + self.diffuse_left.capacity() as u64
                    + self.diffuse_right.capacity() as u64)
                    .saturating_mul(core::mem::size_of::<f32>() as u64),
            )
            .saturating_add(self.shared_diffuse.memory_telemetry().delay_payload_bytes as u64)
            .saturating_add(
                self.environmental_alignment.capacity() as u64 * core::mem::size_of::<f32>() as u64,
            )
            .saturating_add(
                self.echo_history
                    .iter()
                    .map(|history| history.capacity() as u64)
                    .sum::<u64>()
                    .saturating_mul(core::mem::size_of::<f32>() as u64),
            )
    }

    fn process_echo_program(
        &mut self,
        role: EventRole,
        command: SteamMacroIngressCommand,
        input: Option<&[f32]>,
        environmental_bank: &mut [f32],
        environmental_latency_frames: usize,
    ) -> Result<bool, SpatialBackendRenderError> {
        let plan = command.echo_plan;
        if !plan.is_enabled() {
            return Ok(false);
        }
        if usize::from(plan.tap_count) > MAX_MACRO_ECHO_TAPS
            || environmental_bank.len() < 9 * self.block_size_frames
            || input.is_some_and(|plane| plane.len() != self.block_size_frames)
            || environmental_latency_frames > MAX_MACRO_ENVIRONMENTAL_ALIGNMENT_FRAMES
        {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        let role_index = role.index();
        if self.echo_activation_epoch[role_index] != command.activation_epoch {
            self.echo_history[role_index].fill(0.0);
            self.echo_history_head[role_index] = 0;
            self.echo_tail_frames_remaining[role_index] = 0;
            for filter in &mut self.echo_filters[role_index] {
                filter.reset();
            }
            for (tap_index, tap) in plan.taps[..usize::from(plan.tap_count)]
                .iter()
                .copied()
                .enumerate()
            {
                if !tap.active || !tap.delay_samples.is_finite() || tap.delay_samples < 0.0 {
                    return Err(SpatialBackendRenderError::InvalidOutputMetadata);
                }
                let Some(coefficients) = n3d_acn_order2_from_arrival(tap.arrival_direction_enu)
                else {
                    return Err(SpatialBackendRenderError::InvalidOutputMetadata);
                };
                self.echo_coefficients[role_index][tap_index] = coefficients;
                if !self.echo_filters[role_index][tap_index]
                    .set_pressure_gains(tap.spectral_pressure_gain)
                {
                    return Err(SpatialBackendRenderError::InvalidOutputMetadata);
                }
            }
            self.echo_activation_epoch[role_index] = command.activation_epoch;
        }

        let history_len = self.echo_history[role_index].len();
        let maximum_delay = plan.maximum_delay_frames() as usize + environmental_latency_frames;
        if maximum_delay + 1 >= history_len {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        let has_input_energy = input.is_some_and(|plane| plane.iter().any(|sample| *sample != 0.0));
        if has_input_energy {
            self.echo_tail_frames_remaining[role_index] =
                maximum_delay.saturating_add(self.sample_rate_hz as usize * 2);
        } else {
            self.echo_tail_frames_remaining[role_index] =
                self.echo_tail_frames_remaining[role_index].saturating_sub(self.block_size_frames);
        }

        let echo_input_gain = command.runtime_distance_gain();
        if !echo_input_gain.is_finite() || echo_input_gain < 0.0 {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        let history = &mut self.echo_history[role_index];
        let filters = &mut self.echo_filters[role_index];
        let mut head = self.echo_history_head[role_index];
        for frame in 0..self.block_size_frames {
            // Runtime already owns the composed macro-atmosphere transfer. The
            // wrapper owns the remaining route-exact distance scalar, so the
            // authored echo must receive it once just like the detailed stem.
            history[head] = input.map_or(0.0, |plane| plane[frame] * echo_input_gain);
            for (tap_index, tap) in plan.taps[..usize::from(plan.tap_count)]
                .iter()
                .copied()
                .enumerate()
            {
                let coefficients = self.echo_coefficients[role_index][tap_index];
                let delay = tap.delay_samples + environmental_latency_frames as f32;
                let integral = delay.floor() as usize;
                let fraction = delay - integral as f32;
                let newer = (head + history_len - integral) % history_len;
                let older = (newer + history_len - 1) % history_len;
                let delayed = history[newer] * (1.0 - fraction) + history[older] * fraction;
                let shaped = filters[tap_index].process_sample(delayed);
                for (plane, coefficient) in coefficients.into_iter().enumerate() {
                    environmental_bank[plane * self.block_size_frames + frame] +=
                        shaped * coefficient;
                }
            }
            head += 1;
            if head == history_len {
                head = 0;
            }
        }
        self.echo_history_head[role_index] = head;
        Ok(true)
    }

    fn process_diffuse_acn0(
        &mut self,
        environmental_bank: &mut [f32],
        environmental_latency_frames: usize,
    ) -> Result<(), SpatialBackendRenderError> {
        self.diffuse_left.fill(0.0);
        self.diffuse_right.fill(0.0);
        self.shared_diffuse
            .process_block(
                &self.diffuse_send,
                &mut self.diffuse_left,
                &mut self.diffuse_right,
            )
            .map_err(|_| SpatialBackendRenderError::InvalidBlockLength)?;
        let acn0 = environmental_bank
            .get_mut(..self.block_size_frames)
            .ok_or(SpatialBackendRenderError::InvalidBlockLength)?;
        if environmental_latency_frames > MAX_MACRO_ENVIRONMENTAL_ALIGNMENT_FRAMES {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        if self.environmental_latency_frames != environmental_latency_frames {
            self.environmental_alignment.fill(0.0);
            self.environmental_alignment_head = 0;
            self.environmental_latency_frames = environmental_latency_frames;
        }
        let capacity = self.environmental_alignment.len();
        for ((output, left), right) in acn0
            .iter_mut()
            .zip(self.diffuse_left.iter().copied())
            .zip(self.diffuse_right.iter().copied())
        {
            // N3D Y00 is unity. The two decorrelation networks are not spatial
            // left/right channels here; their normalized scalar sum is one
            // world-unrotated omnidirectional ACN0 coefficient.
            let wet = (left + right) * core::f32::consts::FRAC_1_SQRT_2;
            if environmental_latency_frames == 0 {
                *output += wet;
            } else {
                let read = (self.environmental_alignment_head + capacity
                    - environmental_latency_frames)
                    % capacity;
                *output += self.environmental_alignment[read];
                self.environmental_alignment[self.environmental_alignment_head] = wet;
                self.environmental_alignment_head += 1;
                if self.environmental_alignment_head == capacity {
                    self.environmental_alignment_head = 0;
                }
            }
        }
        if self.diffuse_send.iter().any(|sample| *sample != 0.0) {
            self.diffuse_tail_frames_remaining = self
                .diffuse_tail_capacity_frames
                .saturating_add(self.environmental_latency_frames);
        } else {
            self.diffuse_tail_frames_remaining = self
                .diffuse_tail_frames_remaining
                .saturating_sub(self.block_size_frames);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum MacroProgramPlane {
    Original(usize),
    Scaled(EventRole),
}

impl SpatialBackendRenderGraph for MacroIngressSpatialRenderGraph {
    fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
        self.inner.prepare_for_realtime()
    }

    fn render_spatial_block(
        &mut self,
        block: SpatialPropagationRenderBlock<'_>,
    ) -> Result<(), SpatialBackendRenderError> {
        if block.presentation_bank.len() != MAX_SPATIAL_PRESENTATION_FEEDS * self.block_size_frames
        {
            return Err(SpatialBackendRenderError::InvalidBlockLength);
        }
        let routes = self.routes.read();
        let mut plane_kinds = [MacroProgramPlane::Original(0); MAX_ACTIVE_SOURCES];
        let mut source_indices = [0_usize; MAX_ACTIVE_SOURCES];
        let mut plane_counts = [0_usize; MAX_ACTIVE_SOURCES];
        let mut forwarded_count = 0_usize;
        self.diffuse_send.fill(0.0);

        for (input_index, input) in block.sources.iter().enumerate() {
            let role = EventRole::ALL
                .into_iter()
                .find(|role| self.slots.source_index(*role) == input.source_index);
            let Some(role) = role else {
                plane_kinds[forwarded_count] = MacroProgramPlane::Original(input_index);
                source_indices[forwarded_count] = input.source_index;
                plane_counts[forwarded_count] = input.program_plane_count;
                forwarded_count += 1;
                continue;
            };
            let Some(command) = routes.active_command(role) else {
                // Control keeps the reserved Runtime slot held until Swift
                // release/finalize. Once audio has terminally acknowledged it,
                // silence rather than an unconditioned caller plane.
                continue;
            };
            if input.program_plane_count != 1
                || input.program_planes[0].len() != self.block_size_frames
            {
                return Err(SpatialBackendRenderError::InvalidProgramPlaneCount);
            }
            let gain = command.runtime_distance_gain();
            if !gain.is_finite() || gain < 0.0 {
                return Err(SpatialBackendRenderError::InvalidOutputMetadata);
            }
            for (output, sample) in self.scaled_programs[role.index()]
                .iter_mut()
                .zip(input.program_planes[0].iter().copied())
            {
                *output = sample * gain;
            }
            if command.mode == SteamMacroIngressMode::DetailedLocal {
                plane_kinds[forwarded_count] = MacroProgramPlane::Scaled(role);
                source_indices[forwarded_count] = input.source_index;
                plane_counts[forwarded_count] = 1;
                forwarded_count += 1;
            } else if matches!(
                command.mode,
                SteamMacroIngressMode::MacroFallback {
                    shared_diffuse: true
                }
            ) && !self.tail_retiring
            {
                for (send, sample) in self
                    .diffuse_send
                    .iter_mut()
                    .zip(self.scaled_programs[role.index()].iter().copied())
                {
                    *send += sample;
                }
            }
        }

        let forwarded: [SpatialBackendSourceBlock<'_>; MAX_ACTIVE_SOURCES] =
            std::array::from_fn(|index| {
                if index >= forwarded_count {
                    return SpatialBackendSourceBlock {
                        source_index: 0,
                        program_plane_count: 0,
                        program_planes: [&[], &[]],
                    };
                }
                let program_planes = match plane_kinds[index] {
                    MacroProgramPlane::Original(original) => block.sources[original].program_planes,
                    MacroProgramPlane::Scaled(role) => {
                        [&self.scaled_programs[role.index()][..], &[]]
                    }
                };
                SpatialBackendSourceBlock {
                    source_index: source_indices[index],
                    program_plane_count: plane_counts[index],
                    program_planes,
                }
            });
        self.inner
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame: block.block_start_frame,
                propagation_sequence: block.propagation_sequence,
                sources: &forwarded[..forwarded_count],
                presentation_bank: block.presentation_bank,
                environmental_bank: block.environmental_bank,
                metadata: block.metadata,
            })?;

        let mut echo_active = false;
        for role in EventRole::ALL {
            let Some(command) = routes.active_command(role) else {
                continue;
            };
            let input = block
                .sources
                .iter()
                .find(|source| source.source_index == self.slots.source_index(role))
                .map(|source| source.program_planes[0]);
            echo_active |= self.process_echo_program(
                role,
                command,
                input,
                block.environmental_bank,
                block.metadata.environmental_latency_frames as usize,
            )?;
        }

        for role in EventRole::ALL {
            let Some(command) = routes.active_command(role) else {
                continue;
            };
            if command.mode == SteamMacroIngressMode::DetailedLocal {
                continue;
            }
            let source_index = self.slots.source_index(role);
            let feed_index = source_index
                .checked_mul(3)
                .ok_or(SpatialBackendRenderError::InvalidSourceIndex)?;
            if feed_index >= MAX_SPATIAL_PRESENTATION_FEEDS
                || block.metadata.presentation_feeds[feed_index].valid
            {
                return Err(SpatialBackendRenderError::InvalidOutputMetadata);
            }
            let start = feed_index * self.block_size_frames;
            let end = start + self.block_size_frames;
            block.presentation_bank[start..end]
                .copy_from_slice(&self.scaled_programs[role.index()]);
            block.metadata.presentation_feeds[feed_index] = SpatialPresentationFeedMetadata {
                valid: true,
                source_index,
                component: SpatialPresentationComponent::DirectCenter,
                placement: SpatialFeedPlacement::Direction,
                pose_enu: Pose {
                    position: command.local_leg.ingress_proxy_enu,
                    forward: EnuVector3::new(0.0, 1.0, 0.0),
                    up: EnuVector3::new(0.0, 0.0, 1.0),
                },
                direction_enu: command.local_leg.remote_direction_enu,
                latency_frames: u32::try_from(command.local_leg.delay_frames).unwrap_or(u32::MAX),
            };
            block.metadata.active_presentation_feed_count = block
                .metadata
                .active_presentation_feed_count
                .saturating_add(1);
        }
        let diffuse_active = self.diffuse_tail_frames_remaining != 0
            || self.diffuse_send.iter().any(|sample| *sample != 0.0);
        self.process_diffuse_acn0(
            block.environmental_bank,
            block.metadata.environmental_latency_frames as usize,
        )?;
        if diffuse_active && block.metadata.active_environmental_plane_count == 0 {
            block.metadata.active_environmental_order = SpatialAmbisonicOrder::Zero;
            block.metadata.active_environmental_plane_count = 1;
            block.metadata.environmental_channel_order = SpatialAmbisonicChannelOrder::Acn;
            block.metadata.environmental_normalization = SpatialAmbisonicNormalization::N3d;
            block.metadata.environmental_basis = SpatialEnvironmentalBasis::RightHandedEnu;
            block.metadata.world_space_unrotated = true;
        }
        if echo_active {
            if block.metadata.active_environmental_plane_count != 0
                && (block.metadata.environmental_channel_order != SpatialAmbisonicChannelOrder::Acn
                    || block.metadata.environmental_normalization
                        != SpatialAmbisonicNormalization::N3d
                    || block.metadata.environmental_basis
                        != SpatialEnvironmentalBasis::RightHandedEnu
                    || !block.metadata.world_space_unrotated)
            {
                return Err(SpatialBackendRenderError::InvalidOutputMetadata);
            }
            block.metadata.active_environmental_order = SpatialAmbisonicOrder::Two;
            block.metadata.active_environmental_plane_count = 9;
            block.metadata.environmental_channel_order = SpatialAmbisonicChannelOrder::Acn;
            block.metadata.environmental_normalization = SpatialAmbisonicNormalization::N3d;
            block.metadata.environmental_basis = SpatialEnvironmentalBasis::RightHandedEnu;
            block.metadata.world_space_unrotated = true;
        }
        Ok(())
    }

    fn begin_tail_retirement(&mut self) {
        self.tail_retiring = true;
        self.inner.begin_tail_retirement();
    }

    fn tail_retirement_state(&self) -> SpatialTailRetirementState {
        if self.diffuse_tail_frames_remaining != 0
            || self
                .echo_tail_frames_remaining
                .iter()
                .any(|frames| *frames != 0)
            || self.inner.tail_retirement_state() == SpatialTailRetirementState::TailRemaining
        {
            SpatialTailRetirementState::TailRemaining
        } else {
            SpatialTailRetirementState::TailComplete
        }
    }

    fn render_retiring_environmental_tail(
        &mut self,
        environmental_bank: &mut [f32],
    ) -> Result<SpatialTailRetirementState, SpatialBackendRenderError> {
        let inner_state = self
            .inner
            .render_retiring_environmental_tail(environmental_bank)?;
        self.diffuse_send.fill(0.0);
        self.process_diffuse_acn0(environmental_bank, self.environmental_latency_frames)?;
        let routes = self.routes.read();
        for role in EventRole::ALL {
            if let Some(command) = routes.active_command(role) {
                self.process_echo_program(
                    role,
                    command,
                    None,
                    environmental_bank,
                    self.environmental_latency_frames,
                )?;
            }
        }
        if inner_state == SpatialTailRetirementState::TailRemaining
            || self.diffuse_tail_frames_remaining != 0
            || self
                .echo_tail_frames_remaining
                .iter()
                .any(|frames| *frames != 0)
        {
            Ok(SpatialTailRetirementState::TailRemaining)
        } else {
            Ok(SpatialTailRetirementState::TailComplete)
        }
    }
}

/// Exact asset interval needed for one callback block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteamMacroProgramRequest {
    pub event_id: MacroEventId,
    pub role: EventRole,
    pub asset_key: u64,
    pub asset_frame_start: u64,
    pub frame_count: usize,
    pub destination_frame_offset: usize,
    pub block_start_frame: u64,
    pub mode: SteamMacroIngressMode,
}

/// Steam simulation routing for the macro-conditioned program block.
#[derive(Clone, Debug, PartialEq)]
pub struct SteamMacroIngressRoute {
    pub event_id: MacroEventId,
    pub family: IngressEventFamily,
    pub role: EventRole,
    pub source_index: Option<usize>,
    pub source_motion: Option<SourceMotion>,
    pub remote_direction_enu: EnuVector3,
    pub final_local_leg: FinalLocalLeg,
    pub eligibility: EventPropagationEligibility,
    pub detailed_authority: Option<std::sync::Arc<LocalCellAuthority>>,
    pub mode: SteamMacroIngressMode,
}

impl SteamMacroIngressRoute {
    /// Publishes only this reserved source slot. Listener state and all other
    /// logical sources remain under the host's existing authority.
    pub fn apply_to_simulation_update(&self, update: &mut SimulationUpdate) {
        if let (Some(source_index), Some(source_motion)) = (self.source_index, self.source_motion) {
            update.sources[source_index] = source_motion;
        }
    }

    pub fn deactivate_in_simulation_update(&self, update: &mut SimulationUpdate) {
        if let Some(source_index) = self.source_index {
            update.sources[source_index].active = false;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SteamMacroIngressTelemetry {
    pub admitted_events: u64,
    pub detailed_program_blocks: u64,
    pub fallback_program_blocks: u64,
    pub macro_conditioned_samples: u64,
    pub shared_diffuse_send_samples: u64,
    pub released_events: u64,
}

#[derive(Clone, Debug)]
struct ActiveIngress {
    event_id: MacroEventId,
    family: IngressEventFamily,
    role: EventRole,
    asset_key: u64,
    program_seek_frame: u64,
    program_start_frame: u64,
    next_program_frame: u64,
    tail_deadline_frame: u64,
    local_leg: FinalLocalLeg,
    eligibility: EventPropagationEligibility,
    render_authority: IngressRenderAuthority,
    mode: SteamMacroIngressMode,
}

struct RoleProcessor {
    active: Option<ActiveIngress>,
    macro_atmosphere: SpectralTransferFilter,
    local_atmosphere: SpectralTransferFilter,
    macro_distance_gain: f32,
}

impl RoleProcessor {
    fn new(sample_rate_hz: u32) -> Result<Self, SteamMacroIngressError> {
        Ok(Self {
            active: None,
            macro_atmosphere: SpectralTransferFilter::new(sample_rate_hz)
                .map_err(SteamMacroIngressError::SpectralFilter)?,
            local_atmosphere: SpectralTransferFilter::new(sample_rate_hz)
                .map_err(SteamMacroIngressError::SpectralFilter)?,
            macro_distance_gain: 1.0,
        })
    }
}

/// Allocation-free-after-construction consumer for the four event roles.
pub struct SteamMacroIngressConsumer {
    block_size_frames: usize,
    slots: MacroIngressSlotMap,
    roles: [RoleProcessor; EventRole::COUNT],
    shared_diffuse: SharedDiffuseField,
    telemetry: SteamMacroIngressTelemetry,
}

impl SteamMacroIngressConsumer {
    pub fn new(
        sample_rate_hz: u32,
        block_size_frames: usize,
        slots: MacroIngressSlotMap,
        diffuse_profile: DiffuseFieldProfile,
    ) -> Result<Self, SteamMacroIngressError> {
        if sample_rate_hz == 0 {
            return Err(SteamMacroIngressError::InvalidSampleRate);
        }
        if block_size_frames == 0 {
            return Err(SteamMacroIngressError::InvalidBlockSize);
        }
        let roles = [
            RoleProcessor::new(sample_rate_hz)?,
            RoleProcessor::new(sample_rate_hz)?,
            RoleProcessor::new(sample_rate_hz)?,
            RoleProcessor::new(sample_rate_hz)?,
        ];
        let shared_diffuse = SharedDiffuseField::new(sample_rate_hz, diffuse_profile)
            .map_err(SteamMacroIngressError::SharedDiffuse)?;
        Ok(Self {
            block_size_frames,
            slots,
            roles,
            shared_diffuse,
            telemetry: SteamMacroIngressTelemetry::default(),
        })
    }

    /// Consumes the activation and arms its role's monotonic program cursor.
    pub fn admit(
        &mut self,
        activation: LocalIngressActivation,
    ) -> Result<(), SteamMacroIngressError> {
        let role = activation.role;
        if activation.eligibility != role.local_propagation_eligibility() {
            return Err(SteamMacroIngressError::EligibilityMismatch);
        }
        let mode = match &activation.render_authority {
            IngressRenderAuthority::DetailedLocal { .. } => SteamMacroIngressMode::DetailedLocal,
            IngressRenderAuthority::MacroFallback { shared_diffuse, .. } => {
                SteamMacroIngressMode::MacroFallback {
                    shared_diffuse: *shared_diffuse,
                }
            }
        };
        let program_start_frame = match mode {
            SteamMacroIngressMode::DetailedLocal => activation.macro_conditioning.arrival_frame(),
            SteamMacroIngressMode::MacroFallback { .. } => activation
                .fallback_ear_arrival_frame()
                .ok_or(SteamMacroIngressError::InvalidTimeline)?,
        };
        if activation.tail_deadline_frame <= program_start_frame {
            return Err(SteamMacroIngressError::InvalidTimeline);
        }

        let processor = &mut self.roles[role.index()];
        if let Some(previous) = processor.active.as_ref() {
            // MacroLocalIngress may retire the old reservation and publish the
            // next event at this exact frame before the host reaches this
            // consumer. Replacing only a deadline-complete role makes the two
            // valid control-thread call orders equivalent without stealing a
            // still-admitted tail.
            if previous.tail_deadline_frame > activation.macro_conditioning.arrival_frame() {
                return Err(SteamMacroIngressError::RoleAlreadyActive);
            }
            processor.active = None;
            self.telemetry.released_events = self.telemetry.released_events.saturating_add(1);
        }

        processor.macro_atmosphere.reset();
        processor.macro_atmosphere.set_transfer(atmosphere_transfer(
            *activation.macro_conditioning.atmosphere_gain_db(),
        )?);
        processor.local_atmosphere.reset();
        processor.local_atmosphere.set_transfer(atmosphere_transfer(
            activation.local_leg.atmosphere_gain_db,
        )?);
        processor.macro_distance_gain = activation.macro_conditioning.distance_gain();
        processor.active = Some(ActiveIngress {
            event_id: activation.event_id,
            family: activation.family,
            role,
            asset_key: activation.asset_key,
            program_seek_frame: activation.program_seek_frame,
            program_start_frame,
            next_program_frame: program_start_frame,
            tail_deadline_frame: activation.tail_deadline_frame,
            local_leg: activation.local_leg,
            eligibility: activation.eligibility,
            render_authority: activation.render_authority,
            mode,
        });
        self.telemetry.admitted_events = self.telemetry.admitted_events.saturating_add(1);
        Ok(())
    }

    /// Returns the next never-before-consumed asset interval intersecting this
    /// callback. A late callback is an error rather than a silently lost onset.
    pub fn program_request(
        &self,
        role: EventRole,
        block_start_frame: u64,
    ) -> Result<Option<SteamMacroProgramRequest>, SteamMacroIngressError> {
        let Some(active) = self.roles[role.index()].active.as_ref() else {
            return Ok(None);
        };
        let block_end = block_start_frame
            .checked_add(self.block_size_frames as u64)
            .ok_or(SteamMacroIngressError::InvalidTimeline)?;
        if active.next_program_frame < block_start_frame {
            return Err(SteamMacroIngressError::MissedProgramOnset);
        }
        if active.next_program_frame >= block_end
            || active.next_program_frame >= active.tail_deadline_frame
        {
            return Ok(None);
        }
        let request_end = block_end.min(active.tail_deadline_frame);
        let frame_count = usize::try_from(request_end - active.next_program_frame)
            .map_err(|_| SteamMacroIngressError::InvalidTimeline)?;
        let destination_frame_offset =
            usize::try_from(active.next_program_frame - block_start_frame)
                .map_err(|_| SteamMacroIngressError::InvalidTimeline)?;
        let asset_frame_start = active
            .program_seek_frame
            .checked_add(active.next_program_frame - active.program_start_frame)
            .ok_or(SteamMacroIngressError::InvalidTimeline)?;
        Ok(Some(SteamMacroProgramRequest {
            event_id: active.event_id,
            role,
            asset_key: active.asset_key,
            asset_frame_start,
            frame_count,
            destination_frame_offset,
            block_start_frame,
            mode: active.mode,
        }))
    }

    /// Applies disjoint macro and final-local terms into caller-owned buffers.
    /// `program_output` is sent to the Steam source slot for detailed mode or
    /// to the fallback point presenter otherwise. `diffuse_send_mono` is an
    /// accumulator processed once after all roles have contributed.
    pub fn process_request(
        &mut self,
        request: SteamMacroProgramRequest,
        decoded_asset_mono: &[f32],
        program_output: &mut [f32],
        diffuse_send_mono: &mut [f32],
    ) -> Result<SteamMacroIngressRoute, SteamMacroIngressError> {
        if program_output.len() != self.block_size_frames
            || diffuse_send_mono.len() != self.block_size_frames
            || decoded_asset_mono.len() != request.frame_count
        {
            return Err(SteamMacroIngressError::BlockLengthMismatch);
        }
        let expected = self
            .program_request(request.role, request.block_start_frame)?
            .ok_or(SteamMacroIngressError::NoProgramRequest)?;
        if request != expected {
            return Err(SteamMacroIngressError::ProgramRequestMismatch);
        }
        program_output.fill(0.0);
        let processor = &mut self.roles[request.role.index()];
        let active = processor
            .active
            .as_mut()
            .ok_or(SteamMacroIngressError::NoProgramRequest)?;
        let destination = &mut program_output[request.destination_frame_offset
            ..request.destination_frame_offset + request.frame_count];
        for (relative_frame, (input, output)) in decoded_asset_mono
            .iter()
            .copied()
            .zip(destination.iter_mut())
            .enumerate()
        {
            let macro_conditioned =
                processor.macro_atmosphere.process_sample(input) * processor.macro_distance_gain;
            *output = match active.mode {
                SteamMacroIngressMode::DetailedLocal => macro_conditioned,
                SteamMacroIngressMode::MacroFallback { shared_diffuse } => {
                    let local = processor.local_atmosphere.process_sample(macro_conditioned)
                        * active.local_leg.distance_gain;
                    if shared_diffuse {
                        diffuse_send_mono[request.destination_frame_offset + relative_frame] +=
                            local;
                    }
                    local
                }
            };
        }
        active.next_program_frame = active
            .next_program_frame
            .checked_add(request.frame_count as u64)
            .ok_or(SteamMacroIngressError::InvalidTimeline)?;
        self.telemetry.macro_conditioned_samples = self
            .telemetry
            .macro_conditioned_samples
            .saturating_add(request.frame_count as u64);
        match active.mode {
            SteamMacroIngressMode::DetailedLocal => {
                self.telemetry.detailed_program_blocks =
                    self.telemetry.detailed_program_blocks.saturating_add(1);
            }
            SteamMacroIngressMode::MacroFallback { shared_diffuse } => {
                self.telemetry.fallback_program_blocks =
                    self.telemetry.fallback_program_blocks.saturating_add(1);
                if shared_diffuse {
                    self.telemetry.shared_diffuse_send_samples = self
                        .telemetry
                        .shared_diffuse_send_samples
                        .saturating_add(request.frame_count as u64);
                }
            }
        }
        Ok(route_for(active, self.slots))
    }

    /// Advances the shared fallback field exactly once after all role sends for
    /// this block have been summed.
    pub fn render_shared_diffuse(
        &mut self,
        diffuse_send_mono: &[f32],
        output_left: &mut [f32],
        output_right: &mut [f32],
    ) -> Result<(), SteamMacroIngressError> {
        if diffuse_send_mono.len() != self.block_size_frames
            || output_left.len() != self.block_size_frames
            || output_right.len() != self.block_size_frames
        {
            return Err(SteamMacroIngressError::BlockLengthMismatch);
        }
        self.shared_diffuse
            .process_block(diffuse_send_mono, output_left, output_right)
            .map_err(SteamMacroIngressError::SharedDiffuse)
    }

    /// Releases deadline-complete roles back to the queue owner.
    ///
    /// This is safe immediately before or after [`MacroLocalIngress::activate_due`]
    /// at the same frame. If ingress has already retired this event (and may
    /// have admitted its successor), only the consumer's matching old state is
    /// cleared; the successor reservation is left untouched.
    pub fn release_due(
        &mut self,
        current_frame: u64,
        ingress: &mut MacroLocalIngress,
    ) -> Result<usize, SteamMacroIngressError> {
        let mut released = 0;
        for role in EventRole::ALL {
            let due = self.roles[role.index()]
                .active
                .as_ref()
                .is_some_and(|active| active.tail_deadline_frame <= current_frame);
            if !due {
                continue;
            }
            let event_id = self.roles[role.index()]
                .active
                .as_ref()
                .expect("due role is active")
                .event_id;
            let ingress_still_owns_event = ingress
                .admitted_tail(role)
                .is_some_and(|tail| tail.event_id == event_id);
            if ingress_still_owns_event {
                ingress
                    .release(role, event_id)
                    .map_err(SteamMacroIngressError::IngressRelease)?;
            }
            self.roles[role.index()].active = None;
            released += 1;
        }
        self.telemetry.released_events = self
            .telemetry
            .released_events
            .saturating_add(released as u64);
        Ok(released)
    }

    #[must_use]
    pub const fn telemetry(&self) -> SteamMacroIngressTelemetry {
        self.telemetry
    }
}

fn route_for(active: &ActiveIngress, slots: MacroIngressSlotMap) -> SteamMacroIngressRoute {
    let (source_index, source_motion, detailed_authority) = match &active.render_authority {
        IngressRenderAuthority::DetailedLocal { authority } => {
            let source_index = slots.source_index(active.role);
            let direction = active.local_leg.remote_direction_enu;
            let up = if direction.up_m.abs() > 0.99 {
                EnuVector3::new(0.0, 1.0, 0.0)
            } else {
                EnuVector3::new(0.0, 0.0, 1.0)
            };
            (
                Some(source_index),
                Some(SourceMotion {
                    active: true,
                    pose: fightbox_api::Pose {
                        position: active.local_leg.ingress_proxy_enu,
                        forward: direction,
                        up,
                    },
                    linear_velocity_mps: EnuVector3::default(),
                }),
                Some(authority.clone()),
            )
        }
        IngressRenderAuthority::MacroFallback { .. } => (None, None, None),
    };
    SteamMacroIngressRoute {
        event_id: active.event_id,
        family: active.family,
        role: active.role,
        source_index,
        source_motion,
        remote_direction_enu: active.local_leg.remote_direction_enu,
        final_local_leg: active.local_leg,
        eligibility: active.eligibility,
        detailed_authority,
        mode: active.mode,
    }
}

fn atmosphere_transfer(
    atmosphere_gain_db: [f32; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
) -> Result<SpectralTransfer, SteamMacroIngressError> {
    SpectralTransfer::NEUTRAL
        .with_stage(SpectralStage::Atmosphere, atmosphere_gain_db)
        .map_err(SteamMacroIngressError::SpectralTransfer)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteamMacroIngressError {
    InvalidSampleRate,
    InvalidBlockSize,
    InvalidSourceIndex,
    DuplicateSourceIndex,
    ReservedSourceOwnedByHost,
    AssetNotReady,
    RoleAlreadyActive,
    EligibilityMismatch,
    InvalidTimeline,
    MissedProgramOnset,
    NoProgramRequest,
    ProgramRequestMismatch,
    BlockLengthMismatch,
    SpectralFilter(SpectralFilterError),
    SpectralTransfer(SpectralTransferError),
    SharedDiffuse(SharedDiffuseError),
    IngressRelease(EventReleaseError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use fightbox_runtime::{
        CellArtifactIdentity, CellIdentity, FALLBACK_ATMOSPHERE_OBSERVATION, FrozenAtmosphere,
        IngressArrivalContext, IngressFallbackReason, ScheduledMacroEvent,
    };

    const SAMPLE_RATE_HZ: u32 = 48_000;
    const BLOCK_FRAMES: usize = 128;

    struct RecordingSpatialBackend {
        observed_source_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SpatialBackendRenderGraph for RecordingSpatialBackend {
        fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
            Ok(())
        }

        fn render_spatial_block(
            &mut self,
            block: SpatialPropagationRenderBlock<'_>,
        ) -> Result<(), SpatialBackendRenderError> {
            self.observed_source_count
                .store(block.sources.len(), std::sync::atomic::Ordering::Relaxed);
            block.metadata.validity = fightbox_runtime::backend::SpatialOutputValidity::Valid;
            for source in block.sources {
                let feed_index = source.source_index * 3;
                let start = feed_index * source.program_planes[0].len();
                let end = start + source.program_planes[0].len();
                block.presentation_bank[start..end].copy_from_slice(source.program_planes[0]);
                block.metadata.presentation_feeds[feed_index].valid = true;
                block.metadata.presentation_feeds[feed_index].source_index = source.source_index;
                block.metadata.active_presentation_feed_count += 1;
            }
            Ok(())
        }
    }

    fn wrapper_command(mode: SteamMacroIngressMode) -> SteamMacroIngressCommand {
        SteamMacroIngressCommand {
            activation_epoch: 1,
            direct_generation: 2,
            effective_frame: 0,
            event_id: MacroEventId(51),
            family: IngressEventFamily::General {
                atomic_group_id: 51,
            },
            role: EventRole::BallisticCrack,
            asset_key: 500,
            asset_readiness_generation: 8,
            program_seek_frame: 0,
            program_start_frame: 0,
            program_end_frame: 4,
            tail_deadline_frame: 4,
            echo_plan: SteamMacroEchoPlan::OFF,
            authority_world_generation: 1,
            macro_distance_gain: 0.5,
            macro_atmosphere_gain_db: [0.0; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
            local_leg: FinalLocalLeg {
                ingress_proxy_enu: EnuVector3::new(4.0, 5.0, 6.0),
                remote_direction_enu: EnuVector3::new(1.0, 0.0, 0.0),
                distance_m: 4.0,
                delay_frames: 3,
                distance_gain: 0.25,
                atmosphere_gain_db: [0.0; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
            },
            eligibility: EventRole::BallisticCrack.local_propagation_eligibility(),
            mode,
        }
    }

    #[test]
    fn spatial_wrapper_partitions_detailed_and_directional_point_fallback() {
        for (mode, expected_inner_sources, expected_sample) in [
            (SteamMacroIngressMode::DetailedLocal, 1, 0.5_f32),
            (
                SteamMacroIngressMode::MacroFallback {
                    shared_diffuse: false,
                },
                0,
                0.125_f32,
            ),
            (
                SteamMacroIngressMode::MacroFallback {
                    shared_diffuse: true,
                },
                0,
                0.125_f32,
            ),
        ] {
            let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
            let (mut writer, reader) = macro_ingress_render_snapshot_channel();
            let command = wrapper_command(mode);
            let mut routes = MacroIngressRenderSnapshot::default();
            routes.set_active(command);
            writer.publish(routes);
            let mut wrapper = MacroIngressSpatialRenderGraph::new(
                Box::new(RecordingSpatialBackend {
                    observed_source_count: std::sync::Arc::clone(&observed),
                }),
                reader,
                MacroIngressSlotMap::default(),
                SAMPLE_RATE_HZ,
                4,
                DiffuseFieldProfile::SMALL_INTERIOR,
            )
            .unwrap();
            let program = [1.0_f32; 4];
            let source = SpatialBackendSourceBlock {
                source_index: 14,
                program_plane_count: 1,
                program_planes: [&program, &[]],
            };
            let mut presentation = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * 4];
            let mut environmental = [0.0_f32; 9 * 4];
            let mut metadata = fightbox_runtime::backend::SpatialOutputMetadata::default();
            wrapper
                .render_spatial_block(SpatialPropagationRenderBlock {
                    block_start_frame: 0,
                    propagation_sequence: 2,
                    sources: &[source],
                    presentation_bank: &mut presentation,
                    environmental_bank: &mut environmental,
                    metadata: &mut metadata,
                })
                .unwrap();
            assert_eq!(
                observed.load(std::sync::atomic::Ordering::Relaxed),
                expected_inner_sources
            );
            assert!(
                presentation[14 * 3 * 4..(14 * 3 + 1) * 4]
                    .iter()
                    .all(|sample| (*sample - expected_sample).abs() < 1.0e-6)
            );
            assert!(metadata.presentation_feeds[14 * 3].valid);
            if matches!(mode, SteamMacroIngressMode::MacroFallback { .. }) {
                assert_eq!(
                    metadata.presentation_feeds[14 * 3].placement,
                    SpatialFeedPlacement::Direction
                );
                assert_eq!(
                    metadata.presentation_feeds[14 * 3].direction_enu,
                    command.local_leg.remote_direction_enu
                );
            }
            if matches!(
                mode,
                SteamMacroIngressMode::MacroFallback {
                    shared_diffuse: true
                }
            ) {
                assert_eq!(
                    metadata.active_environmental_order,
                    SpatialAmbisonicOrder::Zero
                );
                assert_eq!(metadata.active_environmental_plane_count, 1);
                assert_eq!(
                    metadata.environmental_normalization,
                    SpatialAmbisonicNormalization::N3d
                );
                assert!(metadata.world_space_unrotated);
            }
        }
    }

    #[test]
    fn signed_echo_pressure_filter_preserves_authored_polarity() {
        let mut filter = EchoSpectralPressureFilter::new(SAMPLE_RATE_HZ).unwrap();
        assert!(filter.set_pressure_gains([-0.25; fightbox_api::spectral::SPECTRAL_BAND_COUNT]));
        assert_eq!(filter.process_sample(1.0), -0.25);
        assert_eq!(filter.process_sample(-0.5), 0.125);

        let mixed = [0.5, 0.25, 0.0, -0.25, -0.5, -0.75, -1.0, -0.5];
        assert!(filter.set_pressure_gains(mixed));
        assert_eq!(filter.mode, EchoSpectralPressureMode::Shaped(mixed));
    }

    #[test]
    fn authored_echo_uses_exact_delay_and_world_enu_n3d_acn_order_two() {
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
        let (mut writer, reader) = macro_ingress_render_snapshot_channel();
        let mut command = wrapper_command(SteamMacroIngressMode::DetailedLocal);
        command.echo_plan = SteamMacroEchoPlan {
            tap_count: 1,
            taps: std::array::from_fn(|index| {
                if index == 0 {
                    SteamMacroEchoTap {
                        active: true,
                        delay_samples: 1.0,
                        arrival_direction_enu: EnuVector3::new(-1.0, 0.0, 0.0),
                        spectral_pressure_gain: [1.0; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
                        path_key: [7; 16],
                    }
                } else {
                    SteamMacroEchoTap::default()
                }
            }),
        };
        command.tail_deadline_frame = 96_005;
        let mut routes = MacroIngressRenderSnapshot::default();
        routes.set_active(command);
        writer.publish(routes);
        let mut wrapper = MacroIngressSpatialRenderGraph::new(
            Box::new(RecordingSpatialBackend {
                observed_source_count: observed,
            }),
            reader,
            MacroIngressSlotMap::default(),
            SAMPLE_RATE_HZ,
            4,
            DiffuseFieldProfile::OFF,
        )
        .unwrap();
        let program = [1.0_f32, 0.0, 0.0, 0.0];
        let source = SpatialBackendSourceBlock {
            source_index: 14,
            program_plane_count: 1,
            program_planes: [&program, &[]],
        };
        let mut presentation = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * 4];
        let mut environmental = [0.0_f32; 9 * 4];
        let mut metadata = fightbox_runtime::backend::SpatialOutputMetadata::default();
        wrapper
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame: 0,
                propagation_sequence: 2,
                sources: &[source],
                presentation_bank: &mut presentation,
                environmental_bank: &mut environmental,
                metadata: &mut metadata,
            })
            .unwrap();
        assert_eq!(
            metadata.active_environmental_order,
            SpatialAmbisonicOrder::Two
        );
        assert_eq!(metadata.active_environmental_plane_count, 9);
        assert_eq!(
            metadata.environmental_basis,
            SpatialEnvironmentalBasis::RightHandedEnu
        );
        assert!(metadata.world_space_unrotated);
        assert!((environmental[1] - command.macro_distance_gain).abs() < 1.0e-6);
        assert!(
            (environmental[3 * 4 + 1] - command.macro_distance_gain * 3.0_f32.sqrt()).abs()
                < 1.0e-6
        );
        assert!(
            (environmental[6 * 4 + 1] + command.macro_distance_gain * 0.5 * 5.0_f32.sqrt()).abs()
                < 1.0e-6
        );
        assert!(
            (environmental[8 * 4 + 1] - command.macro_distance_gain * 0.5 * 15.0_f32.sqrt()).abs()
                < 1.0e-6
        );
        for plane in [1_usize, 2, 4, 5, 7] {
            assert_eq!(environmental[plane * 4 + 1], 0.0);
        }
        assert!(wrapper.echo_tail_frames_remaining[EventRole::BallisticCrack.index()] > 0);
    }

    #[test]
    fn shared_fallback_emits_only_world_unrotated_n3d_acn0_and_pins_tail() {
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX));
        let (mut writer, reader) = macro_ingress_render_snapshot_channel();
        let command = wrapper_command(SteamMacroIngressMode::MacroFallback {
            shared_diffuse: true,
        });
        let mut routes = MacroIngressRenderSnapshot::default();
        routes.set_active(command);
        writer.publish(routes);
        let mut wrapper = MacroIngressSpatialRenderGraph::new(
            Box::new(RecordingSpatialBackend {
                observed_source_count: observed,
            }),
            reader,
            MacroIngressSlotMap::default(),
            SAMPLE_RATE_HZ,
            BLOCK_FRAMES,
            DiffuseFieldProfile::SMALL_INTERIOR,
        )
        .unwrap();
        let mut first_nonzero = None;
        for block_index in 0..256 {
            let mut program = [0.0_f32; BLOCK_FRAMES];
            if block_index == 0 {
                program[0] = 1.0;
            }
            let source = SpatialBackendSourceBlock {
                source_index: 14,
                program_plane_count: 1,
                program_planes: [&program, &[]],
            };
            let mut presentation = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_FRAMES];
            let mut environmental = [0.0_f32; 9 * BLOCK_FRAMES];
            let mut metadata = fightbox_runtime::backend::SpatialOutputMetadata::default();
            wrapper
                .render_spatial_block(SpatialPropagationRenderBlock {
                    block_start_frame: (block_index * BLOCK_FRAMES) as u64,
                    propagation_sequence: 2,
                    sources: &[source],
                    presentation_bank: &mut presentation,
                    environmental_bank: &mut environmental,
                    metadata: &mut metadata,
                })
                .unwrap();
            assert!(
                environmental[BLOCK_FRAMES..]
                    .iter()
                    .all(|sample| *sample == 0.0)
            );
            assert_eq!(
                metadata.active_environmental_order,
                SpatialAmbisonicOrder::Zero
            );
            assert_eq!(
                metadata.environmental_channel_order,
                SpatialAmbisonicChannelOrder::Acn
            );
            assert_eq!(
                metadata.environmental_normalization,
                SpatialAmbisonicNormalization::N3d
            );
            assert!(metadata.world_space_unrotated);
            if environmental[..BLOCK_FRAMES]
                .iter()
                .any(|sample| sample.abs() > 1.0e-9)
            {
                first_nonzero = Some(block_index);
                break;
            }
        }
        assert!(first_nonzero.is_some(), "shared ACN0 tail stayed silent");
        wrapper.begin_tail_retirement();
        assert_eq!(
            wrapper.tail_retirement_state(),
            SpatialTailRetirementState::TailRemaining
        );
        let mut retiring = [0.0_f32; 9 * BLOCK_FRAMES];
        assert_eq!(
            wrapper
                .render_retiring_environmental_tail(&mut retiring)
                .unwrap(),
            SpatialTailRetirementState::TailRemaining
        );
    }

    fn authority_with_direct_only_cell(cell: &CellIdentity) -> LocalCellAuthority {
        LocalCellAuthority {
            cell: cell.clone(),
            world_generation: 1,
            package: Some(CellArtifactIdentity::new(cell.clone(), 1, "1".repeat(64))),
            probe_bake: None,
            echo_authority: None,
        }
    }

    fn event(
        id: u64,
        group: u64,
        role: EventRole,
        arrival_frame: u64,
        seek_frame: u64,
        tail_deadline_frame: u64,
    ) -> ScheduledMacroEvent {
        ScheduledMacroEvent {
            event_id: MacroEventId(id),
            atomic_group_id: group,
            role,
            asset_key: 10_000 + id,
            emission_frame: 0,
            ingress_activation_frame: arrival_frame,
            program_seek_frame: seek_frame,
            tail_deadline_frame,
            ingress_position_enu: EnuVector3::new(2.0, 0.0, 0.0),
            remote_bearing_enu: EnuVector3::new(1.0, 0.0, 0.0),
            macro_distance_gain: 0.5,
            macro_atmosphere_gain_db: [0.0; fightbox_api::spectral::SPECTRAL_BAND_COUNT],
            echo_anchor_key: [0; 16],
        }
    }

    fn context<'a>(
        current_frame: u64,
        atmosphere: &'a FrozenAtmosphere,
    ) -> IngressArrivalContext<'a> {
        IngressArrivalContext {
            current_frame,
            sample_rate_hz: SAMPLE_RATE_HZ,
            listener_position_enu: EnuVector3::default(),
            atmosphere,
        }
    }

    fn inactive_simulation_update() -> SimulationUpdate {
        let source = SourceMotion::default();
        SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: source.pose,
                linear_velocity_mps: EnuVector3::default(),
            },
            sources: [source; MAX_ACTIVE_SOURCES],
        }
    }

    #[test]
    fn command_composes_one_runtime_atmosphere_stage_with_route_exact_partition() {
        let base = SpectralTransfer::default()
            .with_stage(SpectralStage::Atmosphere, [-1.0; 8])
            .unwrap()
            .with_stage(SpectralStage::Ground, [2.0; 8])
            .unwrap();
        let mut command = SteamMacroIngressCommand {
            macro_distance_gain: 0.5,
            macro_atmosphere_gain_db: [-3.0; 8],
            ..SteamMacroIngressCommand::default()
        };
        command.local_leg.distance_gain = 0.25;
        command.local_leg.atmosphere_gain_db = [-5.0; 8];

        let detailed = command.compose_runtime_spectral_transfer(base).unwrap();
        assert_eq!(detailed.stage_gain_db(SpectralStage::Atmosphere), [-4.0; 8]);
        assert_eq!(detailed.stage_gain_db(SpectralStage::Ground), [2.0; 8]);
        assert_eq!(detailed.combined_gain_db(), [-2.0; 8]);
        assert_eq!(command.runtime_distance_gain(), 0.5);

        command.mode = SteamMacroIngressMode::MacroFallback {
            shared_diffuse: true,
        };
        let fallback = command.compose_runtime_spectral_transfer(base).unwrap();
        assert_eq!(fallback.stage_gain_db(SpectralStage::Atmosphere), [-9.0; 8]);
        assert_eq!(fallback.stage_gain_db(SpectralStage::Ground), [2.0; 8]);
        assert_eq!(fallback.combined_gain_db(), [-7.0; 8]);
        assert_eq!(command.runtime_distance_gain(), 0.125);
    }

    #[test]
    fn control_overlay_owns_reserved_slots_and_separates_runtime_from_steam_activity() {
        let atmosphere = FrozenAtmosphere::freeze(Some(FALLBACK_ATMOSPHERE_OBSERVATION));
        let cell = CellIdentity::new("chi", "e0:n0");
        let mut ingress =
            MacroLocalIngress::new(cell.clone(), Some(authority_with_direct_only_cell(&cell)));
        ingress
            .admit_group(&[event(31, 23, EventRole::BallisticCrack, 4, 100, 512)])
            .unwrap();
        let mut commands = SteamMacroIngressCommandBatch::from_activations(
            &ingress.activate_due(context(4, &atmosphere)),
            71,
            9,
            4,
        );
        assert!(commands.bind_asset_readiness(EventRole::BallisticCrack, 8));

        let mut fallback_ingress = MacroLocalIngress::new(cell, None);
        fallback_ingress
            .admit_group(&[event(32, 24, EventRole::StandardImpulse, 4, 200, 768)])
            .unwrap();
        let mut fallback_commands = SteamMacroIngressCommandBatch::from_activations(
            &fallback_ingress.activate_due(context(4, &atmosphere)),
            72,
            9,
            4,
        );
        assert!(fallback_commands.bind_asset_readiness(EventRole::StandardImpulse, 10));

        let mut overlay = MacroIngressControlOverlay::default();
        overlay.activate(commands).unwrap();
        overlay.activate(fallback_commands).unwrap();
        let mut update = inactive_simulation_update();
        overlay.validate_complete_host_update(&update).unwrap();
        overlay.overlay_backend_update(&mut update);
        assert!(update.sources[14].active, "detailed crack enters Steam");
        assert!(
            !update.sources[13].active,
            "fallback is excluded from Steam"
        );
        let runtime = overlay.overlay_runtime_activity([false; MAX_ACTIVE_SOURCES]);
        assert!(runtime[14]);
        assert!(runtime[13], "fallback provider remains required by Runtime");

        let mut hostile = inactive_simulation_update();
        hostile.sources[14].active = true;
        assert_eq!(
            overlay.validate_complete_host_update(&hostile),
            Err(SteamMacroIngressError::ReservedSourceOwnedByHost)
        );

        let crack = commands
            .for_role(EventRole::BallisticCrack)
            .copied()
            .unwrap();
        let mut acknowledgement = SteamMacroIngressAcknowledgement {
            kind: SteamMacroIngressAcknowledgementKind::Deactivated,
            activation_epoch: crack.activation_epoch,
            direct_generation: crack.direct_generation,
            audio_frame: crack.tail_deadline_frame,
            event_id: crack.event_id,
            role: crack.role,
            asset_key: crack.asset_key,
            asset_readiness_generation: crack.asset_readiness_generation,
        };
        acknowledgement.activation_epoch += 1;
        assert!(overlay.acknowledge(acknowledgement).is_none());
        assert!(overlay.active_command(EventRole::BallisticCrack).is_some());
        acknowledgement.activation_epoch = crack.activation_epoch;
        assert_eq!(overlay.acknowledge(acknowledgement), Some(crack));
        assert!(overlay.active_command(EventRole::BallisticCrack).is_none());
    }

    #[test]
    fn readiness_and_acknowledgement_handoffs_are_tokened_copy_backpressure() {
        let cell = CellIdentity::new("chi", "chi:e0:n0");
        let atmosphere = FrozenAtmosphere::freeze(None);
        let mut ingress =
            MacroLocalIngress::new(cell.clone(), Some(authority_with_direct_only_cell(&cell)));
        ingress
            .admit_group(&[
                event(41, 19, EventRole::BallisticCrack, 100, 7, 2_000),
                event(42, 19, EventRole::BallisticBlast, 100, 11, 2_100),
            ])
            .unwrap();
        let activations = ingress.activate_due(context(100, &atmosphere));
        let mut commands =
            SteamMacroIngressCommandBatch::from_activations(&activations, 77, 9, 128);
        assert!(!commands.is_asset_ready());
        assert!(!commands.bind_asset_readiness(EventRole::BallisticCrack, 3));
        assert!(commands.bind_asset_readiness(EventRole::BallisticCrack, 8));
        assert!(commands.bind_asset_readiness(EventRole::BallisticBlast, 10));
        assert!(commands.is_asset_ready());
        assert!(!core::mem::needs_drop::<SteamMacroIngressAcknowledgement>());
        assert!(!core::mem::needs_drop::<
            SteamMacroIngressAcknowledgementBatch,
        >());

        let (mut audio_publisher, mut control_receiver) =
            steam_macro_ingress_acknowledgement_channel();
        let mut acknowledgements = SteamMacroIngressAcknowledgementBatch::default();
        for command in commands.iter() {
            assert!(acknowledgements.try_push(SteamMacroIngressAcknowledgement {
                kind: SteamMacroIngressAcknowledgementKind::Activated,
                activation_epoch: command.activation_epoch,
                direct_generation: command.direct_generation,
                audio_frame: command.effective_frame,
                event_id: command.event_id,
                role: command.role,
                asset_key: command.asset_key,
                asset_readiness_generation: command.asset_readiness_generation,
            }));
        }
        assert!(audio_publisher.is_ready());
        audio_publisher.try_publish(acknowledgements).unwrap();
        let retry = SteamMacroIngressAcknowledgementBatch::default();
        assert_eq!(audio_publisher.try_publish(retry), Err(retry));
        let delivered = control_receiver.try_take().unwrap();
        assert_eq!(delivered.len(), 2);
        assert!(delivered.iter().all(|ack| ack.activation_epoch == 77));
        assert!(
            delivered
                .iter()
                .all(|ack| ack.asset_readiness_generation & 1 == 0)
        );
        assert!(audio_publisher.is_ready());
    }

    #[test]
    fn activation_handoff_copies_correlated_commands_and_backpressures_without_loss() {
        assert!(!core::mem::needs_drop::<SteamMacroIngressCommand>());
        assert!(!core::mem::needs_drop::<SteamMacroIngressCommandBatch>());

        let atmosphere = FrozenAtmosphere::freeze(Some(FALLBACK_ATMOSPHERE_OBSERVATION));
        let cell = CellIdentity::new("chi", "e0:n0");
        let mut first_ingress =
            MacroLocalIngress::new(cell.clone(), Some(authority_with_direct_only_cell(&cell)));
        first_ingress
            .admit_group(&[event(1, 1, EventRole::BallisticCrack, 4, 100, 512)])
            .unwrap();
        let mut first = SteamMacroIngressCommandBatch::from_activations(
            &first_ingress.activate_due(context(4, &atmosphere)),
            7,
            9,
            4,
        );
        assert!(first.bind_asset_readiness(EventRole::BallisticCrack, 8));

        let mut second_ingress =
            MacroLocalIngress::new(cell.clone(), Some(authority_with_direct_only_cell(&cell)));
        second_ingress
            .admit_group(&[event(2, 2, EventRole::StandardImpulse, 4, 200, 512)])
            .unwrap();
        let mut second = SteamMacroIngressCommandBatch::from_activations(
            &second_ingress.activate_due(context(4, &atmosphere)),
            8,
            10,
            4,
        );
        assert!(second.bind_asset_readiness(EventRole::StandardImpulse, 10));

        let (mut publisher, mut receiver) = steam_macro_ingress_activation_channel();
        assert!(publisher.is_ready());
        assert!(publisher.try_reserve());
        assert!(!publisher.is_ready());
        publisher.publish_reserved(first).unwrap();
        assert!(!publisher.is_ready());
        let second = publisher.try_publish(second).unwrap_err();

        let received = receiver.try_take().unwrap();
        let crack = received.for_role(EventRole::BallisticCrack).unwrap();
        assert_eq!(crack.event_id, MacroEventId(1));
        assert_eq!(crack.activation_epoch, 7);
        assert_eq!(crack.direct_generation, 9);
        assert_eq!(crack.effective_frame, 4);
        assert_eq!(crack.authority_world_generation, 1);
        assert_eq!(crack.program_seek_frame, 100);
        assert_eq!(crack.program_start_frame, 4);
        assert_eq!(crack.mode, SteamMacroIngressMode::DetailedLocal);

        assert!(publisher.is_ready());
        publisher.try_publish(second).unwrap();
        let received = receiver.try_take().unwrap();
        let fallback = received.for_role(EventRole::StandardImpulse).unwrap();
        assert_eq!(fallback.event_id, MacroEventId(2));
        assert_eq!(fallback.activation_epoch, 8);
        assert_eq!(fallback.direct_generation, 10);
        assert_eq!(
            fallback.mode,
            SteamMacroIngressMode::MacroFallback {
                shared_diffuse: true,
            }
        );
        assert_eq!(
            fallback.program_start_frame,
            4 + fallback.local_leg.delay_frames
        );
        assert!(receiver.try_take().is_none());
    }

    #[test]
    fn macro_conditioning_reaches_detailed_and_audible_fallback_once_with_stable_timing() {
        let atmosphere = FrozenAtmosphere::freeze(Some(FALLBACK_ATMOSPHERE_OBSERVATION));
        let cell = CellIdentity::new("chi", "e0:n0");
        let mut ingress =
            MacroLocalIngress::new(cell.clone(), Some(authority_with_direct_only_cell(&cell)));
        let crack = event(1, 1, EventRole::BallisticCrack, 4, 100, 512);
        let standard = event(2, 1, EventRole::StandardImpulse, 4, 200, 512);
        let next_crack = event(3, 2, EventRole::BallisticCrack, 792, 300, 920);
        ingress.admit_group(&[crack, standard]).unwrap();
        ingress.admit_group(&[next_crack]).unwrap();

        let mut consumer = SteamMacroIngressConsumer::new(
            SAMPLE_RATE_HZ,
            BLOCK_FRAMES,
            MacroIngressSlotMap::default(),
            DiffuseFieldProfile::SMALL_INTERIOR,
        )
        .unwrap();
        for activation in ingress
            .activate_due(context(4, &atmosphere))
            .into_activations()
        {
            consumer.admit(activation).unwrap();
        }

        let crack_request = consumer
            .program_request(EventRole::BallisticCrack, 0)
            .unwrap()
            .unwrap();
        assert_eq!(crack_request.asset_frame_start, 100);
        assert_eq!(crack_request.destination_frame_offset, 4);
        let mut detailed_output = [0.0; BLOCK_FRAMES];
        let mut diffuse_send = [0.0; BLOCK_FRAMES];
        let crack_route = consumer
            .process_request(
                crack_request,
                &vec![1.0; crack_request.frame_count],
                &mut detailed_output,
                &mut diffuse_send,
            )
            .unwrap();
        assert_eq!(crack_route.source_index, Some(14));
        assert_eq!(crack_route.final_local_leg.distance_gain, 0.5);
        assert!(!crack_route.eligibility.baked_reflections);
        assert_eq!(detailed_output[..4], [0.0; 4]);
        assert!(detailed_output[4..].iter().all(|sample| *sample == 0.5));
        assert!(diffuse_send.iter().all(|sample| *sample == 0.0));

        let fallback_request = consumer
            .program_request(EventRole::StandardImpulse, 256)
            .unwrap()
            .unwrap();
        assert_eq!(fallback_request.asset_frame_start, 200);
        assert_eq!(fallback_request.destination_frame_offset, 28);
        let mut fallback_output = [0.0; BLOCK_FRAMES];
        let fallback_route = consumer
            .process_request(
                fallback_request,
                &vec![1.0; fallback_request.frame_count],
                &mut fallback_output,
                &mut diffuse_send,
            )
            .unwrap();
        assert_eq!(
            fallback_route.mode,
            SteamMacroIngressMode::MacroFallback {
                shared_diffuse: true
            }
        );
        assert!(matches!(fallback_route.detailed_authority, None));
        assert!(matches!(
            ingress.telemetry().last_fallback_reason,
            Some(IngressFallbackReason::MissingProbeBakeAuthority)
        ));
        let expected_fallback = 0.5
            * fallback_route.final_local_leg.distance_gain
            * 10.0_f32.powf(fallback_route.final_local_leg.atmosphere_gain_db[0] / 20.0);
        assert_eq!(fallback_output[..28], [0.0; 28]);
        assert!(
            fallback_output[28..]
                .iter()
                .all(|sample| (*sample - expected_fallback).abs() < 1.0e-6)
        );
        assert!(diffuse_send[..28].iter().all(|sample| *sample == 0.0));
        assert!(
            diffuse_send[28..]
                .iter()
                .all(|sample| (*sample - expected_fallback).abs() < 1.0e-6)
        );

        // Exercise the valid opposite host order at the exact shared
        // deadline: ingress advances first, then the consumer releases its old
        // signal state without releasing the newly reserved crack.
        let next = ingress.activate_due(context(792, &atmosphere));
        assert_eq!(
            next.for_role(EventRole::BallisticCrack).unwrap().event_id,
            MacroEventId(3)
        );
        assert_eq!(consumer.release_due(792, &mut ingress).unwrap(), 2);
        assert_eq!(
            ingress
                .admitted_tail(EventRole::BallisticCrack)
                .unwrap()
                .event_id,
            MacroEventId(3)
        );
        consumer
            .admit(next.into_activations().next().unwrap())
            .unwrap();
        assert_eq!(
            consumer
                .program_request(EventRole::BallisticCrack, 768)
                .unwrap()
                .unwrap()
                .destination_frame_offset,
            24
        );
    }
}
