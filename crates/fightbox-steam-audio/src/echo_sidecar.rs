//! Wave 14 deterministic image-source echo sidecar contracts.
//!
//! Eligibility and trigger time are authored outside the audio callback. A
//! source is structurally off unless its fixture opts in and its loop asset
//! supplies at least one onset. The fixed-capacity profile below carries those
//! onsets as sample frames; the callback advances only a deterministic loop
//! phase and never consults wall time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use fightbox_api::{EnuVector3, ImpulseClass};
use fightbox_runtime::backend::MAX_ACTIVE_SOURCES;

use crate::{ReflectionQualityLevel, SourceQualityLevel, SteamVector3};

pub const MAX_ECHO_TAPS_PER_SOURCE: usize = 4;
pub const MAX_ECHO_TAPS_GLOBAL: usize = 8;
pub const MAX_ECHO_ONSETS: usize = 32;

/// Provisional, ear-ratified NLOS corner losses in Steam Audio's three bands:
/// low 0–0.8 kHz, mid 0.8–8 kHz, high 8–22 kHz. These remain named constants
/// so a later listening gate can retune them without changing the table shape.
pub const CORNER_LOSS_DB_LOW: f32 = -9.0;
pub const CORNER_LOSS_DB_MID: f32 = -15.0;
pub const CORNER_LOSS_DB_HIGH: f32 = -24.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EchoProfileError {
    EmptyLoop,
    TooManyOnsets,
    OnsetOutsideLoop,
    OnsetsNotStrictlyAscending,
}

/// Immutable per-source onset schedule carried by `MultiSourceDescriptor`.
///
/// `Off` is represented structurally by `onset_count == 0`, so descriptor
/// absence and an asset without `onsets_s` take the exact same bypass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EchoProfile {
    loop_frames: u32,
    onset_frames: [u32; MAX_ECHO_ONSETS],
    onset_count: u8,
    impulse_class: ImpulseClass,
}

impl EchoProfile {
    pub const OFF: Self = Self {
        loop_frames: 0,
        onset_frames: [0; MAX_ECHO_ONSETS],
        onset_count: 0,
        impulse_class: ImpulseClass::None,
    };

    /// Builds an enabled loop profile from descriptor-derived sample frames.
    pub fn from_loop_frames(
        loop_frames: u32,
        onset_frames: &[u32],
        impulse_class: ImpulseClass,
    ) -> Result<Self, EchoProfileError> {
        if loop_frames == 0 {
            return Err(EchoProfileError::EmptyLoop);
        }
        if onset_frames.is_empty() {
            return Ok(Self::OFF);
        }
        if onset_frames.len() > MAX_ECHO_ONSETS {
            return Err(EchoProfileError::TooManyOnsets);
        }
        let mut fixed = [0; MAX_ECHO_ONSETS];
        let mut previous = None;
        for (index, onset) in onset_frames.iter().copied().enumerate() {
            if onset >= loop_frames {
                return Err(EchoProfileError::OnsetOutsideLoop);
            }
            if previous.is_some_and(|value| onset <= value) {
                return Err(EchoProfileError::OnsetsNotStrictlyAscending);
            }
            fixed[index] = onset;
            previous = Some(onset);
        }
        Ok(Self {
            loop_frames,
            onset_frames: fixed,
            onset_count: onset_frames.len() as u8,
            impulse_class,
        })
    }

    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.onset_count != 0
    }

    pub(crate) const fn loop_frames(self) -> u32 {
        self.loop_frames
    }

    pub(crate) const fn onset_count(self) -> usize {
        self.onset_count as usize
    }

    pub(crate) const fn onset_at(self, index: usize) -> u32 {
        self.onset_frames[index]
    }

    pub(crate) const fn impulse_class(self) -> ImpulseClass {
        self.impulse_class
    }
}

impl Default for EchoProfile {
    fn default() -> Self {
        Self::OFF
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EchoPathKind {
    #[default]
    Specular,
    Diffraction,
}

/// One host-planned discrete echo path, in local ENU metres.
///
/// The backend converts it with the same law as its analytic plan: spherical
/// spreading and air loss once over `physical_path_length_m`,
/// `band_pressure_gain` once, and the provisional corner voicing on top for a
/// diffraction path. The host owns path validity and the excess-delay window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EchoPathGeometry {
    pub kind: EchoPathKind,
    pub stable_path_id: u32,
    /// Length actually traveled; keys the gain law and impulse shaping.
    pub physical_path_length_m: f32,
    /// Length whose `/ c` is the rendered delay. It differs from the physical
    /// length when the host re-references the echo to the primary the renderer
    /// actually plays, e.g. straight-line-timed pathing for an NLOS route
    /// without a published [`PrimaryRoute`].
    pub render_delay_path_m: f32,
    /// Final bounce point or diffracting edge point; the tap arrives from here.
    pub arrival_position_enu: EnuVector3,
    /// Per-band (low, mid, high) pressure product of every surface
    /// interaction on the path; `[1.0; 3]` for a bare edge.
    pub band_pressure_gain: [f32; 3],
}

/// The rendered primary arrival that a host plan's echoes trail.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EchoPrimary {
    /// The direct stage renders the primary; free-field tap levels already
    /// trail it by the extra spreading, air, and interaction loss.
    LineOfSight,
    /// Baked pathing renders the primary along a street route this long. The
    /// taps inherit its actual Steam transfer at the trigger block.
    Routed { path_length_m: f32 },
}

/// A host-validated primary street route. While one is published for a
/// source, its baked-pathing stage plays `length_m / c` late instead of on the
/// straight line; `topology_id` names the route's vertex chain, so a change of
/// route fades instead of gliding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrimaryRoute {
    pub length_m: f32,
    pub topology_id: u64,
}

/// One rendered tap exactly as the backend will freeze it on a trigger.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlannedEchoTap {
    pub kind: EchoPathKind,
    pub stable_path_id: u32,
    pub physical_path_length_m: f32,
    pub delay_samples: f32,
    /// Free-field law, rendered when no routed primary transfer is known.
    pub distance_gain: f32,
    pub band_gain: [f32; 3],
    /// Per band, this tap over the routed primary it trails:
    /// `(L_p / L_e) · air(L_e − L_p) · interactions`.
    pub primary_relative_gain: Option<[f32; 3]>,
}

/// The taps of one published external plan, strongest predicted pressure first.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlannedEchoTaps {
    pub count: u8,
    pub taps: [PlannedEchoTap; MAX_ECHO_TAPS_PER_SOURCE],
}

impl PlannedEchoTaps {
    #[must_use]
    pub fn as_slice(&self) -> &[PlannedEchoTap] {
        &self.taps[..usize::from(self.count)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EchoPlanError {
    SourceOutOfRange,
    SourceEchoDisabled,
    /// Non-finite or non-positive length, a delay path beyond the delay line,
    /// a band gain outside `[0, 1]`, or a path shorter than its routed primary.
    /// For a primary route, a length beyond the propagation horizon.
    InvalidPath,
}

/// Lock-free per-source trigger generations shared by the host's audio-thread
/// input provider and the render graph. Zero means "never triggered".
pub(crate) struct EchoTriggerGenerations {
    generations: [AtomicU64; MAX_ACTIVE_SOURCES],
    /// Sources whose trigger found no routed primary transfer and kept the
    /// free-field law, one bit per source, until the host takes them.
    transfer_fallbacks: AtomicU32,
}

impl EchoTriggerGenerations {
    pub(crate) fn new() -> Self {
        Self {
            generations: std::array::from_fn(|_| AtomicU64::new(0)),
            transfer_fallbacks: AtomicU32::new(0),
        }
    }

    pub(crate) fn load(&self, source_index: usize) -> u64 {
        self.generations[source_index].load(Ordering::Acquire)
    }

    pub(crate) fn note_transfer_fallback(&self, source_index: usize) {
        self.transfer_fallbacks
            .fetch_or(1 << source_index, Ordering::Relaxed);
    }
}

/// Callback-safe producer of explicit echo trigger generations.
///
/// Store a new generation for a source in the same audio callback that feeds
/// its shot's first dry sample; the render graph then freezes that source's
/// published plan for this block. One atomic store, no lock or allocation.
#[derive(Clone)]
pub struct EchoTrigger {
    generations: Arc<EchoTriggerGenerations>,
}

impl EchoTrigger {
    pub(crate) fn new(generations: Arc<EchoTriggerGenerations>) -> Self {
        Self { generations }
    }

    /// Ignores an out-of-range index rather than panicking on the callback.
    pub fn trigger(&self, source_index: usize, generation: u64) {
        if let Some(slot) = self.generations.generations.get(source_index) {
            slot.store(generation, Ordering::Release);
        }
    }

    /// Control thread: sources (one bit each) whose triggers since the last
    /// call rendered free-field taps because their routed primary had no
    /// baked-path transfer yet.
    pub fn take_transfer_fallbacks(&self) -> u32 {
        self.generations
            .transfer_fallbacks
            .swap(0, Ordering::Relaxed)
    }
}

/// A host-supplied plan, published whole for every source. `present == false`
/// keeps the backend's own analytic plan for that source.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ExternalEchoPlan {
    pub(crate) present: bool,
    pub(crate) plan: EchoSourcePlan,
}

/// One control-side path candidate copied into the render snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct EchoTapPlan {
    pub(crate) valid: bool,
    pub(crate) kind: EchoPathKind,
    pub(crate) stable_path_id: u32,
    pub(crate) total_path_distance_m: f32,
    pub(crate) delay_samples: f32,
    pub(crate) arrival_position: SteamVector3,
    pub(crate) distance_gain: f32,
    pub(crate) band_gain: [f32; 3],
    pub(crate) score: f32,
    /// A host tap trailing a routed primary: when the trigger block knows that
    /// primary's transfer, `primary_relative_gain` scales it instead of the
    /// free-field `distance_gain` and `band_gain`.
    pub(crate) inherits_primary: bool,
    pub(crate) primary_relative_gain: [f32; 3],
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct EchoSourcePlan {
    pub(crate) generation: u64,
    pub(crate) taps: [EchoTapPlan; MAX_ECHO_TAPS_PER_SOURCE],
    pub(crate) tap_count: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EchoTapBudget {
    pub(crate) per_source: usize,
    pub(crate) global: usize,
}

pub(crate) const fn tap_budget(level: ReflectionQualityLevel) -> EchoTapBudget {
    match level {
        ReflectionQualityLevel::Full => EchoTapBudget {
            per_source: 4,
            global: 8,
        },
        ReflectionQualityLevel::Reduced | ReflectionQualityLevel::Intermediate => EchoTapBudget {
            per_source: 2,
            global: 4,
        },
        ReflectionQualityLevel::Minimum => EchoTapBudget {
            per_source: 1,
            global: 2,
        },
    }
}

/// Deterministically selects delivered prefixes across all eligible sources.
/// Each next candidate competes by predicted pressure, total distance, stable
/// path ID, then stable source index. NLOS plans put their reserved corner tap
/// first before entering this graph-wide selection.
pub(crate) fn delivered_tap_counts(
    profiles: &[EchoProfile; MAX_ACTIVE_SOURCES],
    plans: &[EchoSourcePlan; MAX_ACTIVE_SOURCES],
    active: &[bool; MAX_ACTIVE_SOURCES],
    source_quality: &[SourceQualityLevel; MAX_ACTIVE_SOURCES],
    source_count: usize,
    reflection_level: ReflectionQualityLevel,
) -> [u8; MAX_ACTIVE_SOURCES] {
    let budget = tap_budget(reflection_level);
    let mut delivered = [0_u8; MAX_ACTIVE_SOURCES];
    for _ in 0..budget.global {
        let mut best_source: Option<usize> = None;
        for source_index in 0..source_count.min(MAX_ACTIVE_SOURCES) {
            if !active[source_index]
                || !profiles[source_index].is_enabled()
                || source_quality[source_index] != SourceQualityLevel::Full
            {
                continue;
            }
            let next = usize::from(delivered[source_index]);
            if next >= budget.per_source || next >= usize::from(plans[source_index].tap_count) {
                continue;
            }
            let candidate = plans[source_index].taps[next];
            if !candidate.valid {
                continue;
            }
            let precedes = best_source.is_none_or(|best_index| {
                let best_next = usize::from(delivered[best_index]);
                let best = plans[best_index].taps[best_next];
                candidate
                    .score
                    .total_cmp(&best.score)
                    .reverse()
                    .then_with(|| {
                        candidate
                            .total_path_distance_m
                            .total_cmp(&best.total_path_distance_m)
                    })
                    .then_with(|| candidate.stable_path_id.cmp(&best.stable_path_id))
                    .then_with(|| source_index.cmp(&best_index))
                    .is_lt()
            });
            if precedes {
                best_source = Some(source_index);
            }
        }
        let Some(source_index) = best_source else {
            break;
        };
        delivered[source_index] += 1;
    }
    delivered
}

/// Sample-accurate, wall-clock-free onset detector for one composed loop.
///
/// The onset table is validated strictly ascending, so the scheduler caches
/// the index of the next onset at or after the current loop phase. Each
/// advance is then a single compare-and-increment instead of a scan over all
/// authored onsets. The cache is re-derived on triggers and loop wraps; the
/// fired frames are identical to rescanning the whole table every sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EchoLoopScheduler {
    phase: u32,
    next_onset_index: usize,
}

impl EchoLoopScheduler {
    pub(crate) fn reset(&mut self) {
        self.phase = 0;
        self.next_onset_index = 0;
    }

    /// Returns true when the current sample is a descriptor-authored onset,
    /// then advances and wraps the loop phase by exactly one frame.
    pub(crate) fn advance_sample(&mut self, profile: EchoProfile) -> bool {
        debug_assert!(profile.is_enabled());
        let index = self.next_onset_index;
        let onset = index < profile.onset_count() && profile.onset_at(index) == self.phase;
        self.phase += 1;
        let wrapped = self.phase == profile.loop_frames();
        if wrapped {
            self.phase = 0;
        }
        if wrapped {
            // The restarted phase sits below every onset, so the first entry
            // is again the next candidate.
            self.next_onset_index = 0;
        } else if onset {
            self.next_onset_index = if index + 1 < profile.onset_count() {
                index + 1
            } else {
                0
            };
        }
        onset
    }

    #[cfg(test)]
    const fn phase(self) -> u32 {
        self.phase
    }
}

/// Preallocated shared dry-mono history with fractional read heads.
pub(crate) struct EchoDelayRing {
    samples: Vec<f32>,
    history_dirty: bool,
    write: usize,
}

impl EchoDelayRing {
    pub(crate) fn new(maximum_delay_samples: usize) -> Self {
        Self {
            samples: vec![0.0; maximum_delay_samples.saturating_add(4)],
            history_dirty: false,
            write: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        if self.history_dirty {
            self.samples.fill(0.0);
            self.history_dirty = false;
        }
        self.write = 0;
    }

    pub(crate) fn push(&mut self, sample: f32) {
        self.samples[self.write] = sample;
        self.history_dirty |= sample.to_bits() != 0;
        self.write += 1;
        if self.write == self.samples.len() {
            self.write = 0;
        }
    }

    pub(crate) fn read(&self, delay_samples: f32) -> f32 {
        let delay = delay_samples.clamp(0.0, (self.samples.len() - 3) as f32);
        let whole = delay.floor() as usize;
        let fraction = delay - whole as f32;
        let newest = if self.write == 0 {
            self.samples.len() - 1
        } else {
            self.write - 1
        };
        let first = (newest + self.samples.len() - whole % self.samples.len()) % self.samples.len();
        let second = if first == 0 {
            self.samples.len() - 1
        } else {
            first - 1
        };
        self.samples[first] + (self.samples[second] - self.samples[first]) * fraction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(onsets: &[u32]) -> EchoProfile {
        EchoProfile::from_loop_frames(10, onsets, ImpulseClass::None).unwrap()
    }

    #[test]
    fn profile_rejects_invalid_onset_tables_and_absence_is_off() {
        assert_eq!(
            EchoProfile::from_loop_frames(10, &[], ImpulseClass::None).unwrap(),
            EchoProfile::OFF
        );
        assert_eq!(
            EchoProfile::from_loop_frames(10, &[3, 3], ImpulseClass::None),
            Err(EchoProfileError::OnsetsNotStrictlyAscending)
        );
        assert_eq!(
            EchoProfile::from_loop_frames(10, &[10], ImpulseClass::None),
            Err(EchoProfileError::OnsetOutsideLoop)
        );
    }

    #[test]
    fn scheduler_fires_at_exact_frames_and_across_loop_wrap() {
        let profile = profile(&[0, 4, 9]);
        let mut scheduler = EchoLoopScheduler::default();
        let fired = (0..13)
            .filter(|_| scheduler.advance_sample(profile))
            .collect::<Vec<_>>();
        assert_eq!(fired, [0, 4, 9, 10]);
        assert_eq!(scheduler.phase(), 3);
    }

    /// The cached next-onset index must fire exactly where a full per-sample
    /// rescan of the onset table fires, for every loop length and every
    /// strictly ascending onset layout (including empty tables and multiple
    /// onsets inside one block).
    #[test]
    fn cached_scheduler_matches_full_onset_rescan_for_every_layout() {
        for loop_frames in 1_u32..=9 {
            for mask in 0_u32..(1 << loop_frames) {
                let onsets: Vec<u32> = (0..loop_frames)
                    .filter(|frame| mask & (1 << frame) != 0)
                    .collect();
                if onsets.is_empty() {
                    // An empty table is structurally OFF and never reaches
                    // the scheduler (debug-asserted enabled).
                    continue;
                }
                let profile =
                    EchoProfile::from_loop_frames(loop_frames, &onsets, ImpulseClass::None)
                        .unwrap();
                let mut cached = EchoLoopScheduler::default();
                let mut rescanned_phase = 0_u32;
                let samples = 3 * loop_frames + 2;
                for sample in 0..samples {
                    let expected = (0..profile.onset_count())
                        .any(|index| profile.onset_at(index) == rescanned_phase);
                    assert_eq!(
                        cached.advance_sample(profile),
                        expected,
                        "loop={loop_frames} onsets={onsets:?} sample={sample}"
                    );
                    rescanned_phase += 1;
                    if rescanned_phase == profile.loop_frames() {
                        rescanned_phase = 0;
                    }
                }
            }
        }
    }

    #[test]
    fn governor_transitions_deliver_4_2_1_and_enforce_global_cap_8() {
        let mut profiles = [EchoProfile::OFF; MAX_ACTIVE_SOURCES];
        let mut plans = [EchoSourcePlan::default(); MAX_ACTIVE_SOURCES];
        for index in 0..3 {
            profiles[index] = profile(&[0]);
            plans[index].tap_count = 4;
            for tap_index in 0..4 {
                plans[index].taps[tap_index] = EchoTapPlan {
                    valid: true,
                    score: 1.0 - index as f32 * 0.1 - tap_index as f32 * 0.01,
                    stable_path_id: (index * 4 + tap_index) as u32,
                    ..EchoTapPlan::default()
                };
            }
        }
        let quality = [SourceQualityLevel::Full; MAX_ACTIVE_SOURCES];
        let active = [true; MAX_ACTIVE_SOURCES];
        assert_eq!(
            delivered_tap_counts(
                &profiles,
                &plans,
                &active,
                &quality,
                3,
                ReflectionQualityLevel::Full,
            )[..3],
            [4, 4, 0]
        );
        assert_eq!(
            delivered_tap_counts(
                &profiles,
                &plans,
                &active,
                &quality,
                3,
                ReflectionQualityLevel::Reduced
            )[..3],
            [2, 2, 0]
        );
        assert_eq!(
            delivered_tap_counts(
                &profiles,
                &plans,
                &active,
                &quality,
                3,
                ReflectionQualityLevel::Minimum
            )[..3],
            [1, 1, 0]
        );
    }

    #[test]
    fn direct_only_and_virtualized_sources_receive_no_taps() {
        let mut profiles = [EchoProfile::OFF; MAX_ACTIVE_SOURCES];
        let mut plans = [EchoSourcePlan::default(); MAX_ACTIVE_SOURCES];
        profiles[0] = profile(&[0]);
        profiles[1] = profile(&[0]);
        plans[0].tap_count = 4;
        plans[1].tap_count = 4;
        for plan in &mut plans[..2] {
            for tap in &mut plan.taps {
                tap.valid = true;
                tap.score = 1.0;
            }
        }
        let mut quality = [SourceQualityLevel::Full; MAX_ACTIVE_SOURCES];
        let active = [true; MAX_ACTIVE_SOURCES];
        quality[0] = SourceQualityLevel::DirectOnly;
        quality[1] = SourceQualityLevel::Virtualized;
        assert_eq!(
            delivered_tap_counts(
                &profiles,
                &plans,
                &active,
                &quality,
                2,
                ReflectionQualityLevel::Full,
            )[..2],
            [0, 0]
        );
    }

    #[test]
    fn delay_ring_changes_only_the_additive_branch_alignment() {
        let input = [1.0_f32, 2.0, 3.0, 4.0];
        let direct = input;
        let mut ring = EchoDelayRing::new(16);
        let mut echo = [0.0; 4];
        for (index, sample) in input.into_iter().enumerate() {
            ring.push(sample);
            echo[index] = ring.read(2.0);
        }
        assert_eq!(direct, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(echo, [0.0, 0.0, 1.0, 2.0]);
        assert_eq!(direct.len(), echo.len());
    }

    #[test]
    fn corner_voicing_constants_are_the_ratified_values() {
        assert_eq!(
            [CORNER_LOSS_DB_LOW, CORNER_LOSS_DB_MID, CORNER_LOSS_DB_HIGH,],
            [-9.0, -15.0, -24.0]
        );
    }
}
