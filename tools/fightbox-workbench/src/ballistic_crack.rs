//! Supersonic crack companion for a fixture source's ballistic flight.
//!
//! A source with a `ballistic` block owns one extra point-source slot that is
//! inactive between shots. At a Listen press the control thread plans the
//! shot for the listener's current position, writes the crack N-wave into a
//! preallocated stem bank, and moves the slot to the tangent emission point.
//!
//! The event epoch is the tangent emission time `t*`, so no multi-second
//! flight silence is embedded anywhere. The crack stem starts at the press and
//! the engine adds `r*/c`; the owning source's impact program is delayed by
//! `projectile_end_time - t*` and the engine adds its own propagation delay.
//! Both are shifted by the same activation pre-roll, which covers the latency
//! between publishing the slot's activation and the simulation worker's first
//! active snapshot. Outside the Mach cone the impact plays immediately and no
//! crack is published.
//!
//! A `gunfire` source instead detects the rounds in its looping muzzle asset
//! once at load. Its companion bank contains a complete loop of N-waves on
//! that same emission clock, followed continuously without repeating the
//! activation pre-roll. Listener movement republishes future rounds while
//! preserving the current phase and the next activation horizon.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fightbox_api::ballistics::BallisticMachSegment;
use fightbox_api::{
    AssetAnalysis, AssetMeasurementProvenance, Directivity, EnuVector3, ExtentDescriptor,
    ImpulseClass, Pose, ReferenceLevel, SourceId, SourceProfile,
};
use fightbox_steam_audio::{
    BallisticEventLevels, BallisticPlanOverrides, MultiSourceDescriptor, PiecewiseBallisticShot,
    PiecewiseBallisticShotPlan, SourcePriorityClass, plan_piecewise_ballistic_shot_with,
    synthesize_n_wave_into,
};

use crate::fixture::{FixtureBallistic, FixtureGunfire, FixtureMachSegment, FixtureSource};

/// Silence before every crack and added to every in-cone impact delay.
///
/// Input written while the backend still reports the slot inactive is
/// dropped, so the N-wave must not start before the activation reaches the
/// renderer (one direct tick, or one reflection pass when the single
/// simulation thread is busy). Relative crack/impact timing is unaffected.
pub(crate) const CRACK_PRE_ROLL_SECONDS: f64 = 0.25;
/// Fixed bank length: pre-roll plus any plausible N-wave.
const CRACK_STEM_SECONDS: f64 = 0.5;
/// Early street response only. 125 ms retains the approved first 100 ms
/// of slaps with a small margin; the gun's own reflection IR is unchanged.
const GUN_STREET_IR_SECONDS: f32 = 0.125;
/// Static street geometry refreshes one fifth as often; movement forces a pass.
const GUN_STREET_UPDATE_DIVISOR: u8 = 5;
/// Slot stays active this long after its last delayed sample leaves the engine.
const CRACK_RELEASE_MARGIN_SECONDS: f64 = 0.25;
const BANK_COUNT: usize = 3;
const INDEX_BITS: usize = 2;
const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;
/// Onset threshold relative to the asset's own peak, as in the signed strip.
const ONSET_FRACTION_OF_PEAK: f32 = 1.0e-3;
const CRACK_PROVENANCE: &str = "fightbox-workbench/ballistic-n-wave-audible-segment-rms/v1";

/// One control-thread trigger result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArmedShot {
    pub(crate) plan: PiecewiseBallisticShotPlan,
    /// Frames the owning source's impact program waits after its retrigger.
    pub(crate) impact_delay_frames: u32,
    /// Calibrated crack slot state; `None` outside the Mach cone.
    pub(crate) crack: Option<ArmedCrack>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArmedCrack {
    /// Per-shot profile for output safety: true position and declared level.
    pub(crate) profile: SourceProfile,
    /// Audio blocks after which the slot may be deactivated again.
    pub(crate) release_blocks: u64,
}

/// Fixture flight in planner units.
struct Flight {
    muzzle_position: EnuVector3,
    direction: EnuVector3,
    segments: Vec<BallisticMachSegment>,
    overrides: BallisticPlanOverrides,
}

struct LoopGunfire {
    round_frames: Vec<usize>,
    flights: Vec<Flight>,
    loop_frames: usize,
    generation: u64,
    listener: EnuVector3,
}

impl Flight {
    fn plan(&self, listener_position: EnuVector3) -> Result<PiecewiseBallisticShotPlan, String> {
        plan_piecewise_ballistic_shot_with(
            PiecewiseBallisticShot {
                muzzle_position_enu: self.muzzle_position,
                direction_enu: self.direction,
                segments: &self.segments,
                // No muzzle blast is rendered and the explicit peak anchor
                // replaces the blast-relative crack back-solve.
                levels: BallisticEventLevels {
                    blast_spl_at_one_meter_db: 0.0,
                    crack_over_blast_db_at_reference: 0.0,
                },
            },
            listener_position,
            self.overrides,
        )
        .map_err(|error| format!("{error:?}"))
    }
}

/// Control-thread owner of one ballistic flight and its crack slot.
pub(crate) struct BallisticCrack {
    pub(crate) parent_index: usize,
    pub(crate) slot_index: usize,
    flight: Flight,
    sample_rate_hz: u32,
    block_frames: u32,
    impact_onset_frames: usize,
    /// Load-time calibration of the render-graph slot. Per-shot level
    /// differences are folded into the stem amplitude instead.
    profile: SourceProfile,
    writer: CrackStemWriter,
    scratch: Vec<f32>,
    next_scratch: Vec<f32>,
    wave_scratch: Vec<f32>,
    gun: Option<LoopGunfire>,
    /// Absolute audio block at which the active slot is released.
    pub(crate) release_after_block: Option<u64>,
    summary: String,
    summary_listener: Option<EnuVector3>,
}

/// Everything `load_scene` needs to declare one crack slot.
pub(crate) struct CrackSlotDeclaration {
    pub(crate) crack: BallisticCrack,
    pub(crate) playback: CrackPlayback,
    pub(crate) descriptor: MultiSourceDescriptor,
    pub(crate) profile: SourceProfile,
    pub(crate) pose: Pose,
}

impl BallisticCrack {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn declare(
        parent_index: usize,
        slot_index: usize,
        source: &FixtureSource,
        ballistic: &FixtureBallistic,
        impact_samples: &[f32],
        sample_rate_hz: u32,
        block_frames: u32,
        listener_position: EnuVector3,
    ) -> Result<CrackSlotDeclaration, String> {
        let impact = source.initial_position()?;
        let [east, north, up] = ballistic.muzzle_position_m;
        let muzzle_position = EnuVector3::new(east as f32, north as f32, up as f32);
        let flight = Flight {
            muzzle_position,
            direction: EnuVector3::new(
                impact.east_m - muzzle_position.east_m,
                impact.north_m - muzzle_position.north_m,
                impact.up_m - muzzle_position.up_m,
            ),
            segments: ballistic
                .mach_segments
                .iter()
                .map(|segment| BallisticMachSegment {
                    length_m: segment.length_m,
                    mach: segment.mach,
                })
                .collect(),
            overrides: BallisticPlanOverrides {
                n_wave_reference_duration_ms: ballistic.n_wave_ms_at_30_m,
                crack_peak_db_at_reference: Some(ballistic.crack_peak_db_at_30_m),
            },
        };

        // Calibrate the slot from the load-time listener's plan when it has a
        // crack, else from the anchor at the 30 m reference geometry. Either
        // way the rendered level is corrected per shot.
        let plan = flight
            .plan(listener_position)
            .map_err(|error| format!("source {} ballistic plan failed: {error}", source.id))?;
        let (position, spl_at_one_meter_db) = match plan.crack {
            Some(crack) => (crack.position_enu, crack.spl_at_one_meter_db),
            None => (
                impact,
                fightbox_api::ballistics::crack_spl_at_one_meter_db(
                    ballistic.crack_peak_db_at_30_m
                        - fightbox_api::ballistics::n_wave_crest_factor_db(),
                    0.0,
                    fightbox_api::ballistics::WHITHAM_REFERENCE_DISTANCE_M,
                ),
            ),
        };
        let reference_duration_ms = ballistic
            .n_wave_ms_at_30_m
            .unwrap_or(fightbox_api::ballistics::N_WAVE_REFERENCE_DURATION_MS);
        let stem_frames = (CRACK_STEM_SECONDS * f64::from(sample_rate_hz)).round() as usize;
        let mut scratch = vec![0.0; stem_frames];
        let program_rms_dbfs =
            synthesize_n_wave_into(reference_duration_ms, sample_rate_hz, 0, &mut scratch)
                .map_err(|error| format!("source {} crack synthesis: {error:?}", source.id))?;
        let (writer, reader) = crack_stem_channel(stem_frames);
        let crack = Self {
            parent_index,
            slot_index,
            flight,
            sample_rate_hz,
            block_frames,
            impact_onset_frames: leading_onset_frames(impact_samples),
            profile: crack_profile(
                &source.id,
                position,
                spl_at_one_meter_db as f32,
                program_rms_dbfs,
            )?,
            writer,
            scratch,
            next_scratch: Vec::new(),
            wave_scratch: Vec::new(),
            gun: None,
            release_after_block: None,
            summary: String::new(),
            summary_listener: None,
        };
        let descriptor = MultiSourceDescriptor::at(position)
            .with_reference_level(crack.profile.reference_level)
            .with_impulse_class(ImpulseClass::None)
            .with_initially_active(false)
            .with_source_priority(SourcePriorityClass::TransientEvent)
            // Wave 14 echo sidecar, section beta: a crack is a direct-path
            // event; it sends nothing to reflections and carries no echo.
            .with_reflection_send(false);
        let profile = crack.profile.clone();
        let pose = profile.pose;
        let playback = CrackPlayback {
            parent_index,
            slot_index,
            reader,
        };
        Ok(CrackSlotDeclaration {
            crack,
            playback,
            descriptor,
            profile,
            pose,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn declare_gun(
        parent_index: usize,
        slot_index: usize,
        source: &FixtureSource,
        gun: &FixtureGunfire,
        samples: &[f32],
        sample_rate_hz: u32,
        block_frames: u32,
        listener: EnuVector3,
    ) -> Result<CrackSlotDeclaration, String> {
        let muzzle = source.initial_position()?;
        let aim = EnuVector3::new(
            gun.aim_point_m[0] as f32,
            gun.aim_point_m[1] as f32,
            gun.aim_point_m[2] as f32,
        );
        let direction = EnuVector3::new(
            aim.east_m - muzzle.east_m,
            aim.north_m - muzzle.north_m,
            aim.up_m - muzzle.up_m,
        );
        let aim_distance = vector_length(direction);
        let mach = gun.muzzle_velocity_mps / fightbox_api::ballistics::SOUND_SPEED_MPS;
        let mut endpoint = source.clone();
        endpoint.position_m = Some(gun.aim_point_m);
        let fixture_flight = FixtureBallistic {
            muzzle_position_m: [
                f64::from(muzzle.east_m),
                f64::from(muzzle.north_m),
                f64::from(muzzle.up_m),
            ],
            mach_segments: vec![FixtureMachSegment {
                length_m: aim_distance,
                mach,
            }],
            n_wave_ms_at_30_m: gun.n_wave_ms_at_30_m,
            crack_peak_db_at_30_m: gun.crack_peak_db_at_30_m,
            notes: None,
        };
        let mut declaration = Self::declare(
            parent_index,
            slot_index,
            &endpoint,
            &fixture_flight,
            samples,
            sample_rate_hz,
            block_frames,
            listener,
        )?;
        declaration.crack.flight.segments[0].length_m = gun.supersonic_distance_m;
        // The same companion owns an early Steam reflection effect. Artillery
        // remains direct-only; a gun can explicitly opt into the dry comparison.
        declaration.descriptor = declaration.descriptor.with_reflection_send(gun.street_response);
        if gun.street_response {
            declaration.descriptor = declaration.descriptor
                .with_reflection_ir_limit_seconds(GUN_STREET_IR_SECONDS)
                .with_reflection_simulation_ir_limit_seconds(GUN_STREET_IR_SECONDS)
                .with_reflection_update_divisor(GUN_STREET_UPDATE_DIVISOR)
                .with_reflection_share_radius_m(3.0);
        }
        let rounds = detect_round_frames(samples, sample_rate_hz);
        if rounds.is_empty() {
            return Err(format!(
                "source {} gunfire has no detectable round attacks",
                source.id
            ));
        }
        if let Some(offsets) = &gun.round_aim_offsets_m {
            if offsets.len() != rounds.len() {
                return Err(format!(
                    "source {} gunfire has {} aim offsets for {} detected rounds",
                    source.id, offsets.len(), rounds.len()
                ));
            }
        }
        eprintln!(
            "[gunfire] source {}: {} load-detected rounds in {:.6} s (descriptor onsets unchanged)",
            source.id,
            rounds.len(),
            samples.len() as f64 / f64::from(sample_rate_hz)
        );
        let horizontal = f64::from(direction.east_m).hypot(f64::from(direction.north_m));
        let lateral = if horizontal > 0.0 {
            EnuVector3::new(
                (-f64::from(direction.north_m) / horizontal) as f32,
                (f64::from(direction.east_m) / horizontal) as f32,
                0.0,
            )
        } else {
            EnuVector3::new(1.0, 0.0, 0.0)
        };
        let flights = (0..rounds.len())
            .map(|index| {
                // A fixed hash gives reproducible spread without an audio-thread RNG.
                let hash = (index as u32).wrapping_add(1).wrapping_mul(2_654_435_761);
                let spread = (f64::from(hash) / f64::from(u32::MAX) * 2.0 - 1.0) * gun.dispersion_m
                    + gun.round_aim_offsets_m.as_ref().map_or(0.0, |offsets| offsets[index]);
                Flight {
                    muzzle_position: muzzle,
                    direction: EnuVector3::new(
                        direction.east_m + lateral.east_m * spread as f32,
                        direction.north_m + lateral.north_m * spread as f32,
                        direction.up_m,
                    ),
                    segments: vec![BallisticMachSegment {
                        length_m: gun.supersonic_distance_m,
                        mach,
                    }],
                    overrides: declaration.crack.flight.overrides,
                }
            })
            .collect();
        let pre_roll = (CRACK_PRE_ROLL_SECONDS * f64::from(sample_rate_hz)).round() as usize;
        let frames = pre_roll + samples.len();
        let (writer, reader) = crack_stem_channel(frames);
        declaration.crack.writer = writer;
        declaration.playback.reader = reader;
        declaration.crack.scratch = vec![0.0; frames];
        declaration.crack.next_scratch = vec![0.0; frames];
        declaration.crack.wave_scratch =
            vec![0.0; (CRACK_STEM_SECONDS * f64::from(sample_rate_hz)) as usize];
        declaration.crack.gun = Some(LoopGunfire {
            round_frames: rounds,
            flights,
            loop_frames: samples.len(),
            generation: 0,
            listener,
        });
        Ok(declaration)
    }

    pub(crate) fn is_looping(&self) -> bool {
        self.gun.is_some()
    }

    /// Control thread only. Keep the currently playing phase and freeze the
    /// next activation horizon, so walking cannot cut or repeat an imminent
    /// N-wave when the reader adopts its newly prepared bank.
    pub(crate) fn follow_listener(
        &mut self,
        listener: EnuVector3,
    ) -> Result<Option<ArmedShot>, String> {
        let Some(gun) = &self.gun else {
            return Ok(None);
        };
        if gun.generation == 0
            || vector_length(EnuVector3::new(
                listener.east_m - gun.listener.east_m,
                listener.north_m - gun.listener.north_m,
                listener.up_m - gun.listener.up_m,
            )) < 0.25
        {
            return Ok(None);
        }
        self.arm_gun(gun.generation, listener, true).map(Some)
    }

    fn arm_gun(
        &mut self,
        generation: u64,
        listener: EnuVector3,
        preserve: bool,
    ) -> Result<ArmedShot, String> {
        let plan = self.plan(listener)?;
        let gun = self.gun.as_ref().expect("gun loop");
        let rate = f64::from(self.sample_rate_hz);
        let pre_roll = (CRACK_PRE_ROLL_SECONDS * rate).round() as usize;
        // One shared apparent point supplies the bearing and optional street
        // response. Every round's own flight still determines direct arrival,
        // Whitham level and duration; only its spatial origin is shared.
        let anchor = plan
            .crack
            .map(|crack| crack.position_enu)
            // At a cone edge the centreline can miss while a dispersed round
            // still has a tangent. Use that real tangent, never an old distant
            // slot pose whose delay could put the new event before Play.
            .or_else(|| {
                gun.flights.iter().find_map(|flight| {
                    flight.plan(listener).ok()?.crack.map(|crack| crack.position_enu)
                })
            })
            .unwrap_or(self.profile.pose.position);
        let anchor_distance = vector_length(EnuVector3::new(
            anchor.east_m - listener.east_m,
            anchor.north_m - listener.north_m,
            anchor.up_m - listener.up_m,
        ))
        .max(0.01);
        let anchor_delay = anchor_distance / fightbox_api::ballistics::SOUND_SPEED_MPS;
        let ReferenceLevel::SplAtOneMeter { db_spl } = self.profile.reference_level else {
            unreachable!()
        };
        self.next_scratch.fill(0.0);
        let mut max_level = f64::NEG_INFINITY;
        let mut audible = false;
        for (index, (&round_frame, flight)) in gun.round_frames.iter().zip(&gun.flights).enumerate()
        {
            let round = flight.plan(listener)?;
            let Some(crack) = round.crack else {
                continue;
            };
            let onset =
                pre_roll as f64 + round_frame as f64 + (crack.arrival_time_s - anchor_delay) * rate;
            if onset < pre_roll as f64 {
                return Err("gun crack clock precedes muzzle emission".into());
            }
            let onset = onset.round() as usize;
            let wave_frames =
                fightbox_steam_audio::n_wave_frames(round.n_wave_duration_ms, self.sample_rate_hz);
            if onset + wave_frames > self.next_scratch.len() {
                return Err("gun crack flight exceeds the loop's terminal silence".into());
            }
            let rms = synthesize_n_wave_into(
                round.n_wave_duration_ms,
                self.sample_rate_hz,
                0,
                &mut self.wave_scratch,
            )
            .map_err(|error| format!("gun crack synthesis: {error:?}"))?;
            let virtual_level = crack.spl_at_one_meter_db
                + 20.0
                    * (anchor_distance
                        / (crack.engine_propagation_delay_s
                            * fightbox_api::ballistics::SOUND_SPEED_MPS))
                        .log10();
            max_level = max_level.max(virtual_level);
            let gain_db = virtual_level - f64::from(db_spl)
                + f64::from(self.profile.asset_analysis.program_rms_dbfs)
                - f64::from(rms);
            let gain = 10.0_f64.powf(gain_db / 20.0) as f32;
            for (to, from) in self.next_scratch[onset..onset + wave_frames]
                .iter_mut()
                .zip(&self.wave_scratch)
            {
                *to += *from * gain;
            }
            if !preserve {
                eprintln!(
                    "[gunfire] round={} onset={:.6}s miss={:.3}m crack={:.6}s muzzle={:.6}s gap={:.6}s peak={:.2}dB SPL",
                    index + 1,
                    round_frame as f64 / rate,
                    round.miss_distance_m,
                    CRACK_PRE_ROLL_SECONDS + round_frame as f64 / rate + crack.arrival_time_s,
                    CRACK_PRE_ROLL_SECONDS + round_frame as f64 / rate + round.blast.arrival_time_s,
                    round.blast.arrival_time_s - crack.arrival_time_s,
                    crack.spl_at_one_meter_db
                        - 20.0
                            * (crack.engine_propagation_delay_s
                                * fightbox_api::ballistics::SOUND_SPEED_MPS)
                                .log10()
                        + fightbox_api::ballistics::n_wave_crest_factor_db()
                );
            }
            audible = true;
        }
        if preserve {
            let cursor = self.writer.shared.cursor.load(Ordering::Acquire);
            // Copy a horizon through the loop seam as well. Reading audio never
            // accesses either scratch Vec, only the independently owned banks.
            for delta in 0..pre_roll {
                let absolute = cursor + delta;
                let frame = if absolute < self.scratch.len() {
                    absolute
                } else {
                    pre_roll + (absolute - pre_roll) % gun.loop_frames
                };
                if frame < self.scratch.len() {
                    self.next_scratch[frame] = self.scratch[frame];
                }
            }
        }
        // Frozen samples can retain a louder prior geometry. Safety sees the
        // peak of the complete actual bank, including that horizon, rather
        // than just the newly planned rounds.
        let stem_peak = self
            .next_scratch
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max);
        if stem_peak > 0.0 {
            max_level = max_level.max(f64::from(db_spl) + 20.0 * f64::from(stem_peak).log10());
            audible = true;
        }
        std::mem::swap(&mut self.scratch, &mut self.next_scratch);
        self.writer
            .publish_loop(generation, &self.scratch, pre_roll);
        let gun = self.gun.as_mut().unwrap();
        gun.generation = generation;
        gun.listener = listener;
        self.set_summary(listener, &plan);
        Ok(ArmedShot {
            plan,
            impact_delay_frames: pre_roll as u32,
            crack: audible.then(|| ArmedCrack {
                profile: crack_profile_for_shot(&self.profile, anchor, max_level as f32),
                release_blocks: u64::MAX,
            }),
        })
    }

    pub(crate) fn plan(
        &self,
        listener_position: EnuVector3,
    ) -> Result<PiecewiseBallisticShotPlan, String> {
        self.flight.plan(listener_position)
    }

    pub(crate) fn feed_crack(&self, listener: EnuVector3) -> Option<crate::acoustic_feed::Crack> {
        let plan = if let Some(gun) = &self.gun {
            gun.flights.first()?.plan(listener).ok()?
        } else {
            self.plan(listener).ok()?
        };
        let crack = plan.crack?;
        let tangent = plan.tangent?;
        let emission_time_s = if let Some(gun) = &self.gun {
            CRACK_PRE_ROLL_SECONDS
                + *gun.round_frames.first()? as f64 / f64::from(self.sample_rate_hz)
                + tangent.emission_time_s
        } else {
            CRACK_PRE_ROLL_SECONDS
        };
        Some(crate::acoustic_feed::Crack {
            flight_track_enu_m: [
                self.flight.muzzle_position,
                plan.trajectory_end.position_enu,
            ],
            tangent_position_enu_m: crack.position_enu,
            emission_time_s,
            arrival_time_s: emission_time_s + crack.engine_propagation_delay_s,
            mach: tangent.mach_before,
        })
    }

    /// Plans, synthesizes, and publishes one shot for `generation`, the
    /// owning source's retrigger generation that will start it.
    pub(crate) fn arm(
        &mut self,
        generation: u64,
        listener_position: EnuVector3,
    ) -> Result<ArmedShot, String> {
        if self.is_looping() {
            return self.arm_gun(generation, listener_position, false);
        }
        let plan = self.plan(listener_position)?;
        self.set_summary(listener_position, &plan);
        let Some(crack) = plan.crack else {
            return Ok(ArmedShot {
                plan,
                impact_delay_frames: 0,
                crack: None,
            });
        };
        let sample_rate = f64::from(self.sample_rate_hz);
        let pre_roll_frames = (CRACK_PRE_ROLL_SECONDS * sample_rate).round() as usize;
        let program_rms_dbfs = synthesize_n_wave_into(
            plan.n_wave_duration_ms,
            self.sample_rate_hz,
            pre_roll_frames,
            &mut self.scratch,
        )
        .map_err(|error| format!("crack synthesis: {error:?}"))?;
        // The drive maps the load-time analysis RMS to the load-time level;
        // this gain moves the stem to the shot's level and measured RMS.
        let ReferenceLevel::SplAtOneMeter { db_spl } = self.profile.reference_level else {
            unreachable!("crack slots are declared in SPL at one metre");
        };
        let stem_gain_db = crack.spl_at_one_meter_db - f64::from(db_spl)
            + f64::from(self.profile.asset_analysis.program_rms_dbfs)
            - f64::from(program_rms_dbfs);
        let stem_gain = 10.0_f64.powf(stem_gain_db / 20.0) as f32;
        for sample in &mut self.scratch {
            *sample *= stem_gain;
        }
        let stem_frames = pre_roll_frames
            + fightbox_steam_audio::n_wave_frames(plan.n_wave_duration_ms, self.sample_rate_hz);
        self.writer
            .publish(generation, &self.scratch[..stem_frames]);

        let emission_to_end_s =
            plan.trajectory_end.projectile_time_s - crack.embedded_leading_silence_s;
        let impact_delay_frames = ((emission_to_end_s + CRACK_PRE_ROLL_SECONDS) * sample_rate)
            .round()
            .max(0.0) as usize;
        let impact_delay_frames =
            u32::try_from(impact_delay_frames.saturating_sub(self.impact_onset_frames))
                .map_err(|_| "impact delay exceeds the playback counter".to_owned())?;
        let release_seconds =
            CRACK_STEM_SECONDS + crack.engine_propagation_delay_s + CRACK_RELEASE_MARGIN_SECONDS;
        Ok(ArmedShot {
            plan,
            impact_delay_frames,
            crack: Some(ArmedCrack {
                profile: crack_profile_for_shot(
                    &self.profile,
                    crack.position_enu,
                    crack.spl_at_one_meter_db as f32,
                ),
                release_blocks: (release_seconds * sample_rate / f64::from(self.block_frames))
                    .ceil() as u64,
            }),
        })
    }

    /// One-line header text for the listener position, re-planned only after
    /// the listener has moved.
    pub(crate) fn summary(&mut self, listener_position: EnuVector3) -> &str {
        let moved = self.summary_listener.is_none_or(|previous| {
            let east = previous.east_m - listener_position.east_m;
            let north = previous.north_m - listener_position.north_m;
            let up = previous.up_m - listener_position.up_m;
            east * east + north * north + up * up > 0.25 * 0.25
        });
        if moved {
            match self.plan(listener_position) {
                Ok(plan) => self.set_summary(listener_position, &plan),
                Err(error) => {
                    self.summary = format!("Crack: unavailable ({error})");
                    self.summary_listener = Some(listener_position);
                }
            }
        }
        &self.summary
    }

    fn set_summary(&mut self, listener_position: EnuVector3, plan: &PiecewiseBallisticShotPlan) {
        self.summary = if let Some(gun) = &self.gun {
            match plan.crack {
                Some(crack) => format!(
                    "Crack: {} rounds per loop · {:.0} ms before muzzle report",
                    gun.round_frames.len(),
                    (plan.blast.arrival_time_s - crack.arrival_time_s) * 1000.0
                ),
                None => "Crack: none here (outside the bullet's Mach cone)".to_owned(),
            }
        } else {
            match crack_lead_s(plan) {
                Some(lead_s) => format!("Crack: arrives {lead_s:.2} s before impact"),
                None => "Crack: none here (outside the shell's Mach cone)".to_owned(),
            }
        };
        self.summary_listener = Some(listener_position);
    }
}

/// Seconds by which the crack precedes the impact sound, on the endpoint clock.
pub(crate) fn crack_lead_s(plan: &PiecewiseBallisticShotPlan) -> Option<f64> {
    plan.crack
        .map(|crack| plan.trajectory_end.arrival_time_s - crack.arrival_time_s)
}

fn crack_profile(
    parent_id: &str,
    position: EnuVector3,
    spl_at_one_meter_db: f32,
    program_rms_dbfs: f32,
) -> Result<SourceProfile, String> {
    let provenance = AssetMeasurementProvenance::new(CRACK_PROVENANCE)
        .map_err(|error| format!("invalid crack provenance: {error:?}"))?;
    Ok(SourceProfile {
        id: SourceId::new(format!("{parent_id}-crack")),
        pose: Pose {
            position,
            forward: EnuVector3::new(0.0, 1.0, 0.0),
            up: EnuVector3::new(0.0, 0.0, 1.0),
        },
        reference_level: ReferenceLevel::SplAtOneMeter {
            db_spl: spl_at_one_meter_db,
        },
        // The N-wave's first sample is its +1.0 peak.
        asset_analysis: AssetAnalysis::new(program_rms_dbfs, 0.0, provenance)
            .map_err(|error| format!("invalid crack analysis: {error:?}"))?,
        extent: ExtentDescriptor::Point,
        directivity: Directivity::default(),
        max_speed_mps: 0.0,
    })
}

fn crack_profile_for_shot(
    profile: &SourceProfile,
    position: EnuVector3,
    spl_at_one_meter_db: f32,
) -> SourceProfile {
    let mut shot = profile.clone();
    shot.pose.position = position;
    shot.reference_level = ReferenceLevel::SplAtOneMeter {
        db_spl: spl_at_one_meter_db,
    };
    shot
}

fn vector_length(vector: EnuVector3) -> f64 {
    (f64::from(vector.east_m).powi(2)
        + f64::from(vector.north_m).powi(2)
        + f64::from(vector.up_m).powi(2))
    .sqrt()
}

/// Load-time attacks for dry burst recordings. 1 ms RMS crests, 80 ms
/// non-maximum suppression and a high-frequency attack check reject tails.
/// The detector is independent of the echo descriptor's burst onsets.
pub(crate) fn detect_round_frames(samples: &[f32], rate: u32) -> Vec<usize> {
    let hop = (f64::from(rate) * 0.001).round().max(1.0) as usize;
    let envelope = samples
        .chunks(hop)
        .map(|chunk| {
            (chunk
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>()
                / chunk.len() as f64)
                .sqrt()
        })
        .collect::<Vec<_>>();
    let peak = envelope.iter().copied().fold(0.0_f64, f64::max);
    if peak == 0.0 {
        return Vec::new();
    }
    let mut candidates = (0..envelope.len())
        .filter(|&index| {
            envelope[index] >= peak * 0.48
                && (index == 0 || envelope[index] > envelope[index - 1])
                && (index + 1 == envelope.len() || envelope[index] >= envelope[index + 1])
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|&a, &b| envelope[b].total_cmp(&envelope[a]));
    let mut accepted: Vec<usize> = Vec::new();
    for candidate in candidates {
        if accepted
            .iter()
            .all(|other| candidate.abs_diff(*other) >= 80)
        {
            accepted.push(candidate);
        }
    }
    accepted.sort_unstable();
    let mut attacks = Vec::with_capacity(accepted.len());
    for crest in accepted {
        let mut edge = crest;
        while edge > crest.saturating_sub(20) && envelope[edge - 1] >= envelope[crest] * 0.2 {
            edge -= 1;
        }
        let mut frame = edge * hop;
        let sample_peak = samples[crest * hop..((crest + 1) * hop).min(samples.len())]
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max);
        if let Some(offset) = samples[frame..((edge + 1) * hop).min(samples.len())]
            .iter()
            .position(|sample| sample.abs() >= sample_peak * 0.2)
        {
            frame += offset;
        }
        let attack =
            &samples[frame.saturating_sub(hop * 10)..(frame + hop * 20).min(samples.len())];
        let difference_rms = (attack
            .windows(2)
            .map(|pair| f64::from(pair[1] - pair[0]).powi(2))
            .sum::<f64>()
            / attack.len().saturating_sub(1).max(1) as f64)
            .sqrt();
        attacks.push((frame, difference_rms));
    }
    let attack_peak = attacks
        .iter()
        .map(|(_, peak)| *peak)
        .fold(0.0_f64, f64::max);
    attacks
        .into_iter()
        .filter(|(_, peak)| *peak >= attack_peak * 0.4)
        .map(|(frame, _)| frame)
        .collect()
}

/// First frame at or above a thousandth of the asset's peak.
pub(crate) fn leading_onset_frames(samples: &[f32]) -> usize {
    let peak = samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0_f32, f32::max);
    if peak == 0.0 {
        return 0;
    }
    samples
        .iter()
        .position(|sample| sample.abs() >= peak * ONSET_FRACTION_OF_PEAK)
        .unwrap_or(0)
}

/// Audio-thread player for one crack slot. Only the live-output callback
/// reads it; device-free builds keep it for arming and tests.
#[cfg_attr(not(any(feature = "live-output", test)), allow(dead_code))]
pub(crate) struct CrackPlayback {
    pub(crate) parent_index: usize,
    pub(crate) slot_index: usize,
    reader: CrackStemReader,
}

#[cfg_attr(not(any(feature = "live-output", test)), allow(dead_code))]
impl CrackPlayback {
    pub(crate) fn reset_scene(&mut self) {
        self.reader.playing = false;
        self.reader.cursor = 0;
    }

    /// Reuse a control-prepared stem at the cue's exact sample, including
    /// repeated plays of one ballistic source in the same scene.
    pub(crate) fn fill_scene(
        &mut self,
        prepared_generation: u64,
        play: bool,
        enabled: bool,
        gain: f32,
        output: &mut [f32],
    ) {
        if play {
            self.reader.adopt_latest();
            self.reader.consumed_generation = prepared_generation;
            self.reader.playing = self.reader.bank().generation == prepared_generation;
            self.reader.cursor = 0;
        }
        if !enabled {
            self.reset_scene();
        }
        // A stopped scene must not adopt a prearmed stem without a cue.
        self.reader.consumed_generation = prepared_generation;
        self.reader.fill(prepared_generation, gain, output);
    }

    /// Fills one block. `requested_generation` is the owning source's
    /// retrigger generation from the same mix read that restarts its impact,
    /// so both programs start in the same callback. `gain` is the owner's
    /// enable/mute/solo/monitor gain. Allocation- and lock-free.
    pub(crate) fn fill(&mut self, requested_generation: u64, gain: f32, output: &mut [f32]) {
        self.reader.fill(requested_generation, gain, output);
    }

    pub(crate) fn fill_enabled(
        &mut self,
        generation: u64,
        enabled: bool,
        gain: f32,
        output: &mut [f32],
    ) {
        if !enabled {
            self.reset_scene();
            self.reader.consumed_generation = generation;
        }
        self.fill(generation, gain, output);
    }
}

struct StemBank {
    generation: u64,
    frames: usize,
    samples: Vec<f32>,
    loop_start: Option<usize>,
}

struct Shared {
    banks: [UnsafeCell<StemBank>; BANK_COUNT],
    state: AtomicUsize,
    cursor: AtomicUsize,
}

// SAFETY: the single writer mutates only a bank that is neither published nor
// marked reading. The single audio reader marks a bank through the atomic
// state before reading it and never mutates bank storage.
unsafe impl Sync for Shared {}

fn pack(published: usize, reading: usize) -> usize {
    published | (reading << INDEX_BITS)
}

fn published(state: usize) -> usize {
    state & INDEX_MASK
}

fn reading(state: usize) -> usize {
    (state >> INDEX_BITS) & INDEX_MASK
}

struct CrackStemWriter {
    shared: Arc<Shared>,
}

#[cfg_attr(not(any(feature = "live-output", test)), allow(dead_code))]
struct CrackStemReader {
    shared: Arc<Shared>,
    reading_slot: usize,
    consumed_generation: u64,
    playing: bool,
    cursor: usize,
}

fn crack_stem_channel(capacity_frames: usize) -> (CrackStemWriter, CrackStemReader) {
    let shared = Arc::new(Shared {
        banks: std::array::from_fn(|_| {
            UnsafeCell::new(StemBank {
                generation: 0,
                frames: 0,
                samples: vec![0.0; capacity_frames],
                loop_start: None,
            })
        }),
        state: AtomicUsize::new(pack(0, 0)),
        cursor: AtomicUsize::new(0),
    });
    (
        CrackStemWriter {
            shared: Arc::clone(&shared),
        },
        CrackStemReader {
            shared,
            reading_slot: 0,
            consumed_generation: 0,
            playing: false,
            cursor: 0,
        },
    )
}

impl CrackStemWriter {
    /// Copies one complete stem into a free bank and publishes it. Publish
    /// before the mix that carries `generation`, so the reader always finds
    /// it on its first look.
    fn publish(&mut self, generation: u64, samples: &[f32]) {
        self.publish_with_loop(generation, samples, None);
    }

    fn publish_loop(&mut self, generation: u64, samples: &[f32], loop_start: usize) {
        self.publish_with_loop(generation, samples, Some(loop_start));
    }

    fn publish_with_loop(&mut self, generation: u64, samples: &[f32], loop_start: Option<usize>) {
        let mut state = self.shared.state.load(Ordering::Acquire);
        loop {
            let published_slot = published(state);
            let reading_slot = reading(state);
            let write_slot = (0..BANK_COUNT)
                .find(|slot| *slot != published_slot && *slot != reading_slot)
                .expect("three crack banks leave one control-thread slot");
            // SAFETY: this bank is neither the published bank a reader may
            // adopt nor the bank currently marked as being read.
            let bank = unsafe { &mut *self.shared.banks[write_slot].get() };
            assert!(samples.len() <= bank.samples.len());
            bank.samples[..samples.len()].copy_from_slice(samples);
            bank.frames = samples.len();
            bank.generation = generation;
            bank.loop_start = loop_start;
            match self.shared.state.compare_exchange(
                state,
                pack(write_slot, reading_slot),
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => state = observed,
            }
        }
    }
}

#[cfg_attr(not(any(feature = "live-output", test)), allow(dead_code))]
impl CrackStemReader {
    fn fill(&mut self, requested_generation: u64, gain: f32, output: &mut [f32]) {
        if requested_generation != self.consumed_generation {
            self.consumed_generation = requested_generation;
            self.adopt_latest();
            // An out-of-cone press publishes no bank: its generation never
            // matches, and any earlier crack stops with the new shot.
            self.playing = self.bank().generation == requested_generation;
            self.cursor = 0;
        } else if self.playing && self.bank().loop_start.is_some() {
            // Same-generation motion publication preserves the loop phase.
            self.adopt_latest();
            self.playing = self.bank().generation == requested_generation;
        }
        output.fill(0.0);
        if !self.playing {
            return;
        }
        let mut cursor = self.cursor;
        let bank = self.bank();
        let frames = bank.frames;
        let loop_start = bank.loop_start;
        for sample in output.iter_mut() {
            if cursor >= frames {
                if let Some(start) = loop_start {
                    cursor = start;
                } else {
                    break;
                }
            }
            if cursor >= frames {
                break;
            }
            *sample = bank.samples[cursor] * gain;
            cursor += 1;
        }
        self.cursor = cursor;
        self.shared.cursor.store(cursor, Ordering::Release);
        if cursor >= frames && loop_start.is_none() {
            self.playing = false;
        }
    }

    fn adopt_latest(&mut self) {
        let mut state = self.shared.state.load(Ordering::Acquire);
        loop {
            let published_slot = published(state);
            if published_slot == self.reading_slot {
                return;
            }
            // Only the control thread changes the published index, at UI
            // rate, so a failed exchange retries against a newer bank.
            match self.shared.state.compare_exchange(
                state,
                pack(published_slot, published_slot),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.reading_slot = published_slot;
                    return;
                }
                Err(observed) => state = observed,
            }
        }
    }

    fn bank(&self) -> &StemBank {
        // SAFETY: `reading_slot` is marked in `state` until a later adoption,
        // so the writer cannot select it.
        unsafe { &*self.shared.banks[self.reading_slot].get() }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    use super::*;
    use crate::fixture::Fixture;

    struct CountingAllocator;

    thread_local! {
        static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
        static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
        static DEALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    fn note_allocation() {
        TRACK_ALLOCATIONS.with(|tracking| {
            if tracking.get() {
                ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
            }
        });
    }

    // SAFETY: every operation delegates directly to `System`; the thread-local
    // counter observes calls without changing their allocation semantics.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note_allocation();
            // SAFETY: the caller's layout is forwarded unchanged.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            TRACK_ALLOCATIONS.with(|tracking| {
                if tracking.get() {
                    DEALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                }
            });
            // SAFETY: the pointer and layout came from the delegated allocator.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note_allocation();
            // SAFETY: the caller's layout is forwarded unchanged.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note_allocation();
            // SAFETY: all arguments are forwarded under the realloc contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

    fn count_allocations(operation: impl FnOnce()) -> usize {
        count_allocator_calls(operation).0
    }

    pub(crate) fn count_allocator_calls(operation: impl FnOnce()) -> (usize, usize) {
        DEALLOCATION_COUNT.with(|count| count.set(0));
        ALLOCATION_COUNT.with(|count| count.set(0));
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
        operation();
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
        (
            ALLOCATION_COUNT.with(Cell::get),
            DEALLOCATION_COUNT.with(Cell::get),
        )
    }

    const SPOT_A: EnuVector3 = EnuVector3::new(434.02, 483.82, 1.5);
    const SPOT_B: EnuVector3 = EnuVector3::new(438.02, 483.82, 1.5);
    /// Terminal-segment clock of the reviewed trajectory: 3000 m at 514.5 m/s.
    const IMPACT_TIME_S: f64 = 5.830_904;

    pub(crate) fn street_crack(impact_samples: &[f32]) -> CrackSlotDeclaration {
        let fixture = Fixture::parse(
            include_bytes!("../../../fixtures/city/astra-artillery/street-path-candidate.json"),
            "street-path-candidate.json",
        )
        .unwrap();
        let source = &fixture.sources[0];
        BallisticCrack::declare(
            0,
            1,
            source,
            source.ballistic.as_ref().unwrap(),
            impact_samples,
            48_000,
            128,
            SPOT_A,
        )
        .unwrap()
    }

    fn assert_close(actual: f64, expected: f64, tolerance: f64, what: &str) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{what}: {actual} vs reviewed {expected}"
        );
    }

    #[test]
    fn street_flight_matches_the_reviewed_spot_a_and_b_oracles() {
        let crack = street_crack(&[1.0]).crack;
        // (spot, perpendicular miss b, t*, r*, crack arrival, impact arrival)
        for (spot, miss_m, t_star_s, r_star_m, crack_s, impact_s) in [
            (SPOT_A, 447.613, 4.597_128, 600.536, 6.347_963, 7.304_031),
            (SPOT_B, 449.101, 4.589_044, 602.532, 6.345_698, 7.311_708),
        ] {
            let plan = crack.plan(spot).unwrap();
            let tangent = plan.tangent.unwrap();
            let source = plan.crack.unwrap();
            assert_close(plan.miss_distance_m, miss_m, 0.01, "miss");
            assert_close(tangent.emission_time_s, t_star_s, 1.0e-3, "t*");
            assert_close(tangent.acoustic_distance_m, r_star_m, 0.05, "r*");
            assert_close(source.arrival_time_s, crack_s, 1.0e-3, "crack arrival");
            assert_close(
                plan.trajectory_end.projectile_time_s,
                IMPACT_TIME_S,
                1.0e-3,
                "impact",
            );
            assert_close(
                plan.trajectory_end.arrival_time_s,
                impact_s,
                1.0e-3,
                "impact arrival",
            );
            assert_close(
                crack_lead_s(&plan).unwrap(),
                impact_s - crack_s,
                1.0e-3,
                "crack lead",
            );
            // The engine's own delay supplies exactly r*/c; t* is the epoch.
            assert_close(
                source.engine_propagation_delay_s,
                r_star_m / fightbox_api::ballistics::SOUND_SPEED_MPS,
                1.0e-3,
                "engine delay",
            );
            // Whitham scaling uses the perpendicular miss b, not r*.
            assert_close(
                plan.n_wave_duration_ms,
                2.8 * (miss_m / 30.0).powf(0.25),
                1.0e-3,
                "N-wave",
            );
            // The virtual source is the emission point on the flight line:
            // apparent direction is opposite to wave travel.
            assert_close(
                f64::from(source.position_enu.north_m),
                102.5,
                0.01,
                "on-track",
            );
            assert_close(
                f64::from(source.position_enu.east_m - 102.5),
                f64::from(source.position_enu.up_m - 1.5),
                0.01,
                "45-degree descent",
            );
        }

        for (label, spot) in [("A", SPOT_A), ("B", SPOT_B)] {
            let plan = crack.plan(spot).unwrap();
            let source = plan.crack.unwrap();
            let r_star_m = plan.tangent.unwrap().acoustic_distance_m;
            eprintln!(
                "street_crack spot={label} b={:.3}m t*={:.6}s r*={:.3}m crack={:.6}s impact={:.6}s lead={:.4}s nwave={:.3}ms peak={:.2}dB spl1m_rms={:.2}dB",
                plan.miss_distance_m,
                plan.tangent.unwrap().emission_time_s,
                r_star_m,
                source.arrival_time_s,
                plan.trajectory_end.arrival_time_s,
                crack_lead_s(&plan).unwrap(),
                plan.n_wave_duration_ms,
                source.spl_at_one_meter_db - 20.0 * r_star_m.log10()
                    + fightbox_api::ballistics::n_wave_crest_factor_db(),
                source.spl_at_one_meter_db,
            );
        }

        let plan_a = crack.plan(SPOT_A).unwrap();
        let travel = plan_a.crack_arrival_direction_enu.unwrap();
        assert_close(
            f64::from(-travel.up_m).asin().to_degrees(),
            48.37,
            0.01,
            "apparent elevation",
        );
        // 150.8 dB peak at b = 30 m falls as b^(-3/4).
        let received_peak_db = plan_a.crack.unwrap().spl_at_one_meter_db
            - 20.0 * plan_a.tangent.unwrap().acoustic_distance_m.log10()
            + fightbox_api::ballistics::n_wave_crest_factor_db();
        assert_close(received_peak_db, 133.19, 0.01, "received peak");
    }

    #[test]
    fn in_cone_shot_delays_the_impact_by_its_flight_after_emission() {
        // Impact onset after two frames of residue: the delay compensates it.
        let impact = [0.0, 1.0e-6, 1.0, 0.5];
        let CrackSlotDeclaration { mut crack, .. } = street_crack(&impact);
        let shot = crack.arm(1, SPOT_A).unwrap();
        let t_star_s = shot.plan.tangent.unwrap().emission_time_s;
        let emission_to_end_s = shot.plan.trajectory_end.projectile_time_s - t_star_s;
        let expected = ((emission_to_end_s + CRACK_PRE_ROLL_SECONDS) * 48_000.0).round() as u32 - 2;
        assert_eq!(shot.impact_delay_frames, expected);
        assert_close(emission_to_end_s, 1.233_776, 1.0e-3, "impact start delay");
        let armed = shot.crack.unwrap();
        assert_eq!(
            armed.profile.pose.position,
            shot.plan.crack.unwrap().position_enu
        );
        assert_eq!(crack.summary(SPOT_A), "Crack: arrives 0.96 s before impact");
    }

    #[test]
    fn outside_the_cone_the_impact_plays_immediately_without_a_crack() {
        let CrackSlotDeclaration {
            mut crack,
            mut playback,
            ..
        } = street_crack(&[1.0]);
        // West of the impact the finite flight ends before its tangent point.
        let west = EnuVector3::new(-200.0, 102.5, 1.5);
        crack.arm(1, SPOT_A).unwrap();
        let shot = crack.arm(2, west).unwrap();
        assert!(shot.plan.crack.is_none() && shot.crack.is_none());
        assert_eq!(shot.impact_delay_frames, 0);
        assert_eq!(
            crack.summary(west),
            "Crack: none here (outside the shell's Mach cone)"
        );
        let mut block = [9.0_f32; 128];
        for _ in 0..200 {
            playback.fill_scene(2, true, true, 1.0, &mut block);
            assert!(block.iter().all(|sample| *sample == 0.0));
        }
    }

    #[test]
    fn crack_stem_starts_with_its_generation_after_the_pre_roll() {
        let CrackSlotDeclaration {
            mut crack,
            mut playback,
            ..
        } = street_crack(&[1.0]);
        let shot = crack.arm(1, SPOT_A).unwrap();
        let pre_roll = (CRACK_PRE_ROLL_SECONDS * 48_000.0).round() as usize;
        let wave = fightbox_steam_audio::n_wave_frames(shot.plan.n_wave_duration_ms, 48_000);
        assert_eq!(wave, 264);

        // The owner's previous generation plays nothing.
        let mut block = [9.0_f32; 128];
        playback.fill(0, 1.0, &mut block);
        assert!(block.iter().all(|sample| *sample == 0.0));

        let mut rendered = Vec::new();
        for _ in 0..(pre_roll + wave).div_ceil(128) + 2 {
            playback.fill(1, 0.5, &mut block);
            rendered.extend_from_slice(&block);
        }
        let onset = rendered.iter().position(|sample| *sample != 0.0).unwrap();
        assert_eq!(onset, pre_roll);
        let last = rendered.iter().rposition(|sample| *sample != 0.0).unwrap();
        assert!(last < pre_roll + wave);
        assert!(rendered[onset] > 0.0 && rendered[onset + wave / 2] < 0.0);
    }

    #[test]
    fn bank_publication_is_adopted_once_per_generation() {
        let (mut writer, mut reader) = crack_stem_channel(8);
        let mut block = [0.0_f32; 4];

        writer.publish(1, &[1.0, 2.0, 3.0, 4.0, 5.0]);
        reader.fill(1, 1.0, &mut block);
        assert_eq!(block, [1.0, 2.0, 3.0, 4.0]);
        // A newer publication does not disturb the stem being read.
        writer.publish(2, &[7.0; 8]);
        writer.publish(3, &[8.0; 8]);
        reader.fill(1, 1.0, &mut block);
        assert_eq!(block, [5.0, 0.0, 0.0, 0.0]);
        reader.fill(1, 1.0, &mut block);
        assert_eq!(block, [0.0; 4]);

        // Skipped generations adopt only the latest bank.
        reader.fill(3, 2.0, &mut block);
        assert_eq!(block, [16.0; 4]);
        // An owner retrigger with no crack bank stops the previous crack.
        reader.fill(4, 1.0, &mut block);
        assert_eq!(block, [0.0; 4]);
    }

    #[test]
    fn audio_thread_adoption_and_playback_do_not_allocate() {
        let CrackSlotDeclaration {
            mut crack,
            mut playback,
            ..
        } = street_crack(&[1.0]);
        let mut block = [0.0_f32; 128];
        playback.fill(0, 1.0, &mut block);
        for generation in 1..=4 {
            crack.arm(generation, SPOT_A).unwrap();
            let allocations = count_allocations(|| {
                for _ in 0..200 {
                    playback.fill(generation, 1.0, &mut block);
                }
            });
            assert_eq!(allocations, 0);
            for _ in 0..2 {
                let mut peak = 0.0_f32;
                let allocations = count_allocations(|| {
                    for block_index in 0..200 {
                        playback.fill_scene(generation, block_index == 0, true, 1.0, &mut block);
                        peak = block
                            .iter()
                            .fold(peak, |peak, sample| peak.max(sample.abs()));
                    }
                });
                assert_eq!(allocations, 0);
                assert!(
                    peak > 0.0,
                    "same prepared stem must retrigger for a later cue"
                );
            }
        }
    }

    fn test_gun(samples: &[f32]) -> CrackSlotDeclaration {
        test_gun_with(samples, None, false)
    }

    fn test_gun_with(
        samples: &[f32],
        offsets: Option<Vec<f64>>,
        street_response: bool,
    ) -> CrackSlotDeclaration {
        let mut value = serde_json::json!({
            "fixture_id": "gun-crack-test",
            "listener": {"position_m": [0.0, 195.0, 1.5], "forward_enu": [0.0, 1.0, 0.0]},
            "sources": [{"id": "gun", "asset_id": "squad-m2-burst-loop",
                "position_m": [3.0, 0.0, 1.5], "default_enabled": false, "restart_on_enable": true,
                "reference_level": {"mode": "SplAtOneMeter", "db_spl": 153.0},
                "gunfire": {"aim_point_m": [3.0, 195.0, 1.5], "muzzle_velocity_mps": 890.0,
                    "supersonic_distance_m": 500.0, "dispersion_m": 0.35,
                    "crack_peak_db_at_30_m": 140.0, "n_wave_ms_at_30_m": 0.5}}]
        });
        value["sources"][0]["gunfire"]["street_response"] = serde_json::json!(street_response);
        if let Some(offsets) = offsets {
            value["sources"][0]["gunfire"]["round_aim_offsets_m"] = serde_json::json!(offsets);
            value["sources"][0]["gunfire"]["dispersion_m"] = serde_json::json!(0.0);
        }
        let street: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../../fixtures/city/astra-artillery/street-path-candidate.json"
        ))
        .unwrap();
        value["simulation"] = street["simulation"].clone();
        let fixture = Fixture::parse(&serde_json::to_vec(&value).unwrap(), "test.json").unwrap();
        let source = &fixture.sources[0];
        BallisticCrack::declare_gun(
            0,
            1,
            source,
            source.gunfire.as_ref().unwrap(),
            samples,
            48_000,
            128,
            EnuVector3::new(0.0, 195.0, 1.5),
        )
        .unwrap()
    }

    fn attack_train() -> Vec<f32> {
        let mut signal = vec![0.0; 48_000];
        for onset in [1000, 7000, 13_000] {
            for i in 0..300 {
                signal[onset + i] = if i % 2 == 0 { 0.8 } else { -0.8 };
            }
        }
        signal
    }

    #[test]
    fn authored_round_aims_repeat_and_street_send_preserves_the_direct_stem() {
        let samples = attack_train();
        let offsets = Some(vec![-12.0, -27.0, 0.0]);
        let mut dry = test_gun_with(&samples, offsets.clone(), false);
        let mut street = test_gun_with(&samples, offsets, true);
        assert_eq!(dry.descriptor.with_reflection_send(true)
            .with_reflection_ir_limit_seconds(GUN_STREET_IR_SECONDS)
                .with_reflection_simulation_ir_limit_seconds(GUN_STREET_IR_SECONDS)
            .with_reflection_update_divisor(GUN_STREET_UPDATE_DIVISOR)
            .with_reflection_share_radius_m(3.0), street.descriptor);
        assert_ne!(dry.descriptor, street.descriptor);
        let listener = EnuVector3::new(0.0, 195.0, 1.5);
        dry.crack.arm(1, listener).unwrap();
        street.crack.arm(1, listener).unwrap();
        assert_eq!(dry.crack.scratch, street.crack.scratch);
        let flights = &street.crack.gun.as_ref().unwrap().flights;
        let misses = flights.iter().map(|flight| flight.plan(listener).unwrap().miss_distance_m)
            .collect::<Vec<_>>();
        assert!((14.0..16.0).contains(&misses[0]));
        assert!((28.0..31.0).contains(&misses[1]));
        assert!((2.9..3.1).contains(&misses[2]));
        let mut first = vec![0.0; 12_000 + 48_000];
        let mut next = vec![0.0; 48_000];
        let (allocs, frees) = count_allocator_calls(|| {
            street.playback.fill(1, 1.0, &mut first);
            street.playback.fill(1, 1.0, &mut next);
        });
        assert_eq!((allocs, frees), (0, 0));
        assert_eq!(&first[12_000..], &next);
    }

    #[test]
    fn loop_gun_clock_matches_each_geometry_and_repeats_without_extra_pre_roll() {
        let samples = attack_train();
        let mut declaration = test_gun(&samples);
        let listener = EnuVector3::new(0.0, 195.0, 1.5);
        let armed = declaration.crack.arm(1, listener).unwrap();
        assert_eq!(armed.impact_delay_frames, 12_000);
        let gun = declaration.crack.gun.as_ref().unwrap();
        assert_eq!(gun.round_frames, [1000, 7000, 13_000]);
        let first_round = gun.flights[0].plan(listener).unwrap();
        let feed = declaration.crack.feed_crack(listener).unwrap();
        assert!(
            (feed.arrival_time_s
                - (CRACK_PRE_ROLL_SECONDS + 1000.0 / 48_000.0
                    + first_round.crack.unwrap().arrival_time_s))
                .abs() < 1.0e-9
        );
        let anchor = armed.crack.unwrap().profile.pose.position;
        let delay = vector_length(EnuVector3::new(
            anchor.east_m - listener.east_m,
            anchor.north_m - listener.north_m,
            anchor.up_m - listener.up_m,
        )) / 343.0;
        let expected = gun
            .round_frames
            .iter()
            .zip(&gun.flights)
            .map(|(&onset, flight)| {
                let round = flight.plan(listener).unwrap();
                let crack = round.crack.unwrap();
                assert!(round.blast.arrival_time_s > crack.arrival_time_s);
                (12_000.0 + onset as f64 + (crack.arrival_time_s - delay) * 48_000.0).round()
                    as usize
            })
            .collect::<Vec<_>>();
        let mut output = vec![0.0; 12_000 + 3 * 48_000];
        let (allocs, frees) =
            count_allocator_calls(|| declaration.playback.fill(1, 1.0, &mut output));
        assert_eq!((allocs, frees), (0, 0));
        let attacks = output
            .iter()
            .enumerate()
            .filter(|(i, x)| **x > 0.0 && (*i == 0 || output[*i - 1] <= 0.0))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        for cycle in 0..3 {
            for (index, onset) in expected.iter().enumerate() {
                assert_eq!(attacks[cycle * 3 + index], onset + cycle * 48_000);
            }
        }
    }

    #[test]
    fn walking_replans_loop_phase_without_cutting_imminent_audio_and_stops_cleanly() {
        let samples = attack_train();
        let mut declaration = test_gun(&samples);
        declaration
            .crack
            .arm(1, EnuVector3::new(0.0, 195.0, 1.5))
            .unwrap();
        let mut prefix = vec![0.0; 20_000];
        declaration
            .playback
            .fill_scene(1, true, true, 1.0, &mut prefix);
        let mut horizon = declaration.crack.scratch[20_000..32_000].to_vec();
        declaration
            .crack
            .follow_listener(EnuVector3::new(-20.0, 195.0, 1.5))
            .unwrap()
            .unwrap();
        assert_eq!(&declaration.crack.scratch[20_000..32_000], &horizon);
        let allocations = count_allocator_calls(|| {
            declaration
                .playback
                .fill_scene(1, false, true, 1.0, &mut horizon)
        });
        assert_eq!(allocations, (0, 0));
        assert_eq!(&declaration.crack.scratch[20_000..32_000], &horizon);
        let mut block = [9.0; 128];
        declaration.playback.fill_enabled(1, false, 1.0, &mut block);
        assert!(block.iter().all(|sample| *sample == 0.0));
        declaration
            .playback
            .fill_scene(1, true, true, 1.0, &mut block);
        assert_eq!(declaration.playback.reader.cursor, 128);
    }

    #[test]
    fn loop_gun_has_no_crack_behind_muzzle_or_past_finite_tangent() {
        let samples = attack_train();
        let mut declaration = test_gun(&samples);
        for listener in [
            EnuVector3::new(0.0, -10.0, 1.5),
            EnuVector3::new(0.0, 600.0, 1.5),
        ] {
            let armed = declaration.crack.arm(1, listener).unwrap();
            assert!(armed.crack.is_none());
            assert_eq!(armed.impact_delay_frames, 12_000);
            assert!(
                declaration
                    .crack
                    .scratch
                    .iter()
                    .all(|sample| *sample == 0.0)
            );
        }
        let mach = 890.0_f64 / 343.0;
        let edge = EnuVector3::new(
            (3.0 - (mach * mach - 1.0).sqrt() - 0.0005) as f32,
            1.0,
            1.5,
        );
        assert!(declaration.crack.plan(edge).unwrap().crack.is_none());
        assert!(declaration.crack.gun.as_ref().unwrap().flights.iter()
            .any(|flight| flight.plan(edge).unwrap().crack.is_some()));
        assert!(declaration.crack.arm(2, edge).unwrap().crack.is_some());
    }

    #[test]
    #[ignore = "requires local ignored Squad WAVs"]
    fn load_detected_squad_round_clocks_match_the_composed_bursts() {
        for (asset_id, burst_counts) in [
            ("squad-m2-burst-loop", [6, 11, 4, 9]),
            ("squad-dshk-burst-loop", [8, 5, 10, 7]),
        ] {
            let asset = crate::asset::load_asset(asset_id).unwrap();
            let rounds = detect_round_frames(&asset.samples, 48_000);
            assert_eq!(rounds.len(), 30, "{asset_id}");
            let mut grouped = vec![1];
            for pair in rounds.windows(2) {
                if pair[1] - pair[0] > 48_000 {
                    grouped.push(1);
                } else {
                    *grouped.last_mut().unwrap() += 1;
                }
            }
            assert_eq!(grouped, burst_counts, "{asset_id}");
        }
    }
}
