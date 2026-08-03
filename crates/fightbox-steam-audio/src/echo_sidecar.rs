//! Wave 14 deterministic image-source echo sidecar contracts.
//!
//! Eligibility and trigger time are authored outside the audio callback. A
//! source is structurally off unless its fixture opts in and its loop asset
//! supplies at least one onset. The fixed-capacity profile below carries those
//! onsets as sample frames; the callback advances only a deterministic loop
//! phase and never consults wall time.

use fightbox_api::ImpulseClass;
use fightbox_runtime::backend::MAX_ACTIVE_SOURCES;

use crate::{ReflectionQualityLevel, SourceQualityLevel, SteamVector3};

pub const MAX_ECHO_TAPS_PER_SOURCE: usize = 4;
pub const MAX_ECHO_TAPS_GLOBAL: usize = 8;
pub const MAX_ECHO_ONSETS: usize = 32;

/// Provisional, ear-ratified NLOS corner losses. These remain named constants
/// so a later listening gate can retune them without changing the table shape.
pub const CORNER_LOSS_DB_250_HZ: f32 = -9.0;
pub const CORNER_LOSS_DB_1_KHZ: f32 = -15.0;
pub const CORNER_LOSS_DB_4_KHZ: f32 = -24.0;

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
pub(crate) enum EchoPathKind {
    #[default]
    Specular,
    Diffraction,
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
        ReflectionQualityLevel::Reduced => EchoTapBudget {
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EchoLoopScheduler {
    phase: u32,
}

impl EchoLoopScheduler {
    pub(crate) fn reset(&mut self) {
        self.phase = 0;
    }

    /// Returns true when the current sample is a descriptor-authored onset,
    /// then advances and wraps the loop phase by exactly one frame.
    pub(crate) fn advance_sample(&mut self, profile: EchoProfile) -> bool {
        debug_assert!(profile.is_enabled());
        let onset = (0..profile.onset_count()).any(|index| profile.onset_at(index) == self.phase);
        self.phase += 1;
        if self.phase == profile.loop_frames() {
            self.phase = 0;
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
    write: usize,
}

impl EchoDelayRing {
    pub(crate) fn new(maximum_delay_samples: usize) -> Self {
        Self {
            samples: vec![0.0; maximum_delay_samples.saturating_add(4)],
            write: 0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.samples.fill(0.0);
        self.write = 0;
    }

    pub(crate) fn push(&mut self, sample: f32) {
        self.samples[self.write] = sample;
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
            [
                CORNER_LOSS_DB_250_HZ,
                CORNER_LOSS_DB_1_KHZ,
                CORNER_LOSS_DB_4_KHZ,
            ],
            [-9.0, -15.0, -24.0]
        );
    }
}
