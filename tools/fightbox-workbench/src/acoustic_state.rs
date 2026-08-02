//! Per-source acoustic state made legible in the source panel.
//!
//! Whether a source is audible depends on state the views cannot show: baked
//! path-probe coverage at the source's and the listener's current positions,
//! direct occlusion, and which render stages the governor still runs for that
//! source. This module turns those signals into compact badges and owns the
//! seams for the signals `fightbox-steam-audio` does not publish yet.

use fightbox_api::EnuVector3;
use fightbox_runtime::MAX_ACTIVE_SOURCES;
use fightbox_runtime::backend::{SimulationError, SimulationRunner, SimulationUpdate};
use fightbox_runtime::{SnapshotPublication, SnapshotReader, SnapshotWriter};
use fightbox_steam_audio::{
    GovernorTransitionReason, PathQualityLevel, ReflectionQualityLevel, SourceQualityLevel,
    StageOutputGains, SteamAudioSimulationRunner,
};

/// Whether a queried position sits inside an influencing baked probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ProbeCoverage {
    Covered,
    Uncovered,
    /// No probe-coverage query is wired up, so the workbench must not claim either answer.
    #[default]
    Unknown,
}

/// Control-side seam for "does this ENU position have an influencing probe in
/// the loaded baked batch?".
///
/// Steam Audio 4.8.1 has no C API for enumerating a loaded batch's probes, and
/// `fightbox-steam-audio` keeps its serialized-probe reader crate-private. Until
/// that crate publishes a query, this defaults to [`ProbeCoverage::Unknown`] and
/// the badges render an em dash rather than guessing.
pub(crate) struct ProbeCoverageQuery {
    query: Option<Box<dyn Fn(EnuVector3) -> bool>>,
}

impl ProbeCoverageQuery {
    /// The honest default: no query, every position reports `Unknown`.
    pub(crate) const fn unavailable() -> Self {
        Self { query: None }
    }

    /// The wiring point. Unused until a probe-coverage query exists to pass in.
    #[allow(dead_code)]
    pub(crate) fn from_fn(query: impl Fn(EnuVector3) -> bool + 'static) -> Self {
        Self {
            query: Some(Box::new(query)),
        }
    }

    /// Point-tests against an owned copy of the bake's influence spheres,
    /// decoded once at startup rather than re-parsed on every control tick.
    #[allow(dead_code)]
    pub(crate) fn from_spheres(spheres: Vec<(fightbox_steam_audio::EnuVector3, f32)>) -> Self {
        Self::from_fn(move |position| {
            spheres.iter().any(|(center, radius)| {
                let east = center.x - position.east_m;
                let north = center.y - position.north_m;
                let up = center.z - position.up_m;
                east * east + north * north + up * up <= radius * radius
            })
        })
    }

    pub(crate) fn coverage(&self, position: EnuVector3) -> ProbeCoverage {
        match &self.query {
            Some(query) if query(position) => ProbeCoverage::Covered,
            Some(_) => ProbeCoverage::Uncovered,
            None => ProbeCoverage::Unknown,
        }
    }
}

/// What the session says about one source's render work.
///
/// An unbaked generation has no governor at all, which is different from having
/// not heard from the simulation thread yet: without a governor no source can be
/// demoted or virtualized, so every stage still runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceQuality {
    Unknown,
    Ungoverned,
    Governed(SourceQualityLevel),
}

impl Default for SourceQuality {
    fn default() -> Self {
        Self::Unknown
    }
}

impl SourceQuality {
    fn is_virtualized(self) -> bool {
        self == Self::Governed(SourceQualityLevel::Virtualized)
    }
}

/// Whether one render stage can contribute to this source right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum StageAudibility {
    Audible,
    Silent,
    #[default]
    Unknown,
}

/// Control-side copy of the backend state the badges need.
///
/// Published by [`AcousticTelemetryTap`] from the simulation thread and read on
/// the control tick. Every field is `Copy` so it travels through the existing
/// wait-free snapshot channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AcousticTelemetry {
    /// False until the simulation thread has published once.
    pub(crate) known: bool,
    pub(crate) baked_pathing: bool,
    /// The governor reports nothing for an unbaked generation.
    pub(crate) governor_available: bool,
    pub(crate) source_quality: [SourceQualityLevel; MAX_ACTIVE_SOURCES],
    pub(crate) source_physically_calibrated: [bool; MAX_ACTIVE_SOURCES],
    pub(crate) source_predicted_audibility_db: [f32; MAX_ACTIVE_SOURCES],
    /// Per-source Steam Audio direct occlusion, an audibility fraction:
    /// `1.0` fully clear, `0.0` fully occluded. `None` for inactive slots.
    pub(crate) source_occlusion: [Option<f32>; MAX_ACTIVE_SOURCES],
    pub(crate) source_path_eq: [Option<[f32; 3]>; MAX_ACTIVE_SOURCES],
    pub(crate) source_path_sh_energy: [Option<f32>; MAX_ACTIVE_SOURCES],
    pub(crate) governor: Option<GovernorAcousticTelemetry>,
}

/// Delivered governor state copied into the workbench's wait-free telemetry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GovernorAcousticTelemetry {
    pub(crate) ladder_position: u16,
    pub(crate) reason: GovernorTransitionReason,
    pub(crate) reflection_level: ReflectionQualityLevel,
    pub(crate) reflection_rays: i32,
    pub(crate) reflection_bounces: i32,
    pub(crate) reflection_ir_duration_s: f32,
    pub(crate) reflection_cadence_divisor: u8,
    pub(crate) reflection_output_gain: f32,
    pub(crate) pathing: PathQualityLevel,
    pub(crate) ambisonic_order: i32,
    /// First governor state observed after session construction. This remains
    /// fixed so later degradation cannot rewrite the displayed boot decision.
    pub(crate) observed_boot_ladder_position: u16,
    pub(crate) observed_boot_reflection_level: ReflectionQualityLevel,
    pub(crate) observed_boot_pathing: PathQualityLevel,
    pub(crate) observed_boot_ambisonic_order: i32,
    #[cfg(fightbox_governor_boot_telemetry)]
    pub(crate) boot_reflection_level: ReflectionQualityLevel,
    #[cfg(fightbox_governor_boot_telemetry)]
    pub(crate) boot_predicted_cost_ns: u64,
    #[cfg(fightbox_governor_boot_telemetry)]
    pub(crate) boot_p99_budget_ns: u64,
    #[cfg(fightbox_governor_boot_telemetry)]
    pub(crate) boot_cost_limit_ns: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ObservedGovernorBoot {
    ladder_position: u16,
    reflection_level: ReflectionQualityLevel,
    pathing: PathQualityLevel,
    ambisonic_order: i32,
}

impl AcousticTelemetry {
    pub(crate) const UNKNOWN: Self = Self {
        known: false,
        baked_pathing: false,
        governor_available: false,
        source_quality: [SourceQualityLevel::Full; MAX_ACTIVE_SOURCES],
        source_physically_calibrated: [false; MAX_ACTIVE_SOURCES],
        source_predicted_audibility_db: [0.0; MAX_ACTIVE_SOURCES],
        source_occlusion: [None; MAX_ACTIVE_SOURCES],
        source_path_eq: [None; MAX_ACTIVE_SOURCES],
        source_path_sh_energy: [None; MAX_ACTIVE_SOURCES],
        governor: None,
    };

    fn baked_pathing(self) -> Option<bool> {
        self.known.then_some(self.baked_pathing)
    }

    fn quality(self, source_index: usize) -> SourceQuality {
        if !self.known {
            return SourceQuality::Unknown;
        }
        match self
            .governor_available
            .then(|| self.source_quality.get(source_index).copied())
            .flatten()
        {
            Some(level) => SourceQuality::Governed(level),
            None => SourceQuality::Ungoverned,
        }
    }

    fn occlusion(self, source_index: usize) -> Option<f32> {
        self.source_occlusion.get(source_index).copied().flatten()
    }

    fn physically_calibrated(self, source_index: usize) -> Option<bool> {
        (self.known && self.governor_available)
            .then(|| self.source_physically_calibrated.get(source_index).copied())
            .flatten()
    }

    fn predicted_audibility_db(self, source_index: usize) -> Option<f32> {
        (self.known && self.governor_available)
            .then(|| {
                self.source_predicted_audibility_db
                    .get(source_index)
                    .copied()
            })
            .flatten()
    }

    fn path_eq(self, source_index: usize) -> Option<[f32; 3]> {
        self.source_path_eq.get(source_index).copied().flatten()
    }

    fn path_sh_energy(self, source_index: usize) -> Option<f32> {
        self.source_path_sh_energy
            .get(source_index)
            .copied()
            .flatten()
    }
}

/// Wraps the retained session's simulation half so the UI can observe delivered
/// capabilities and governor rungs.
///
/// `SimulationWorker` takes ownership of a `Box<dyn SimulationRunner>` and hands
/// nothing back, so the concrete `SteamAudioSimulationRunner` accessors are
/// otherwise unreachable once the worker starts. Publication happens after the
/// direct pass on the simulation thread; the audio callback is never involved.
pub(crate) struct AcousticTelemetryTap {
    runner: SteamAudioSimulationRunner,
    writer: SnapshotWriter<AcousticTelemetry>,
    observed_boot: Option<ObservedGovernorBoot>,
}

impl AcousticTelemetryTap {
    pub(crate) fn new(
        runner: SteamAudioSimulationRunner,
    ) -> (Self, SnapshotReader<AcousticTelemetry>) {
        let (writer, reader) = SnapshotPublication::new(AcousticTelemetry::UNKNOWN);
        (
            Self {
                runner,
                writer,
                observed_boot: None,
            },
            reader,
        )
    }

    fn publish(&mut self) {
        let capabilities = self.runner.delivered_world_state().capabilities;
        let governor = self.runner.quality_governor_telemetry();
        let mut telemetry = AcousticTelemetry {
            known: true,
            baked_pathing: capabilities.baked_pathing,
            governor_available: governor.is_some(),
            source_quality: [SourceQualityLevel::Full; MAX_ACTIVE_SOURCES],
            source_physically_calibrated: [false; MAX_ACTIVE_SOURCES],
            source_predicted_audibility_db: [0.0; MAX_ACTIVE_SOURCES],
            source_occlusion: [None; MAX_ACTIVE_SOURCES],
            source_path_eq: [None; MAX_ACTIVE_SOURCES],
            source_path_sh_energy: [None; MAX_ACTIVE_SOURCES],
            governor: None,
        };
        for index in 0..MAX_ACTIVE_SOURCES {
            if let Some(diagnostics) = self
                .runner
                .source_diagnostics(index)
                .filter(|diagnostics| diagnostics.active)
            {
                telemetry.source_occlusion[index] = Some(diagnostics.occlusion);
                telemetry.source_path_eq[index] = Some(diagnostics.path_eq);
                telemetry.source_path_sh_energy[index] = Some(diagnostics.path_sh_energy);
            }
        }
        if let Some(governor) = governor {
            let observed_boot = *self.observed_boot.get_or_insert(ObservedGovernorBoot {
                ladder_position: governor.ladder_position,
                reflection_level: governor.reflections.level,
                pathing: governor.pathing,
                ambisonic_order: governor.ambisonic_order,
            });
            telemetry.governor = Some(GovernorAcousticTelemetry {
                ladder_position: governor.ladder_position,
                reason: governor.reason,
                reflection_level: governor.reflections.level,
                reflection_rays: governor.reflections.rays,
                reflection_bounces: governor.reflections.bounces,
                reflection_ir_duration_s: governor.reflections.ir_duration_s,
                reflection_cadence_divisor: governor.reflections.cadence_divisor,
                reflection_output_gain: governor.reflection_output_gain,
                pathing: governor.pathing,
                ambisonic_order: governor.ambisonic_order,
                observed_boot_ladder_position: observed_boot.ladder_position,
                observed_boot_reflection_level: observed_boot.reflection_level,
                observed_boot_pathing: observed_boot.pathing,
                observed_boot_ambisonic_order: observed_boot.ambisonic_order,
                #[cfg(fightbox_governor_boot_telemetry)]
                boot_reflection_level: governor.boot_reflection_level,
                #[cfg(fightbox_governor_boot_telemetry)]
                boot_predicted_cost_ns: governor.boot_predicted_cost_ns,
                #[cfg(fightbox_governor_boot_telemetry)]
                boot_p99_budget_ns: governor.boot_p99_budget_ns,
                #[cfg(fightbox_governor_boot_telemetry)]
                boot_cost_limit_ns: governor.boot_cost_limit_ns,
            });
            for (index, source) in governor.sources.iter().enumerate() {
                telemetry.source_quality[index] = source.quality;
                telemetry.source_physically_calibrated[index] = source.physically_calibrated;
                telemetry.source_predicted_audibility_db[index] = source.predicted_audibility_db;
            }
        }
        self.writer.publish(telemetry);
    }
}

impl SimulationRunner for AcousticTelemetryTap {
    fn update_inputs(&mut self, update: &SimulationUpdate) {
        self.runner.update_inputs(update);
    }

    fn run_direct(&mut self) -> Result<(), SimulationError> {
        let outcome = self.runner.run_direct();
        self.publish();
        outcome
    }

    fn run_pathing(&mut self) -> Result<(), SimulationError> {
        self.runner.run_pathing()
    }

    fn run_reflections(&mut self) -> Result<(), SimulationError> {
        self.runner.run_reflections()
    }
}

/// Everything the badge row for one source needs, resolved on the control tick.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SourceAcousticState {
    pub(crate) source_probes: ProbeCoverage,
    pub(crate) listener_probes: ProbeCoverage,
    pub(crate) occlusion: Option<f32>,
    pub(crate) direct: StageAudibility,
    pub(crate) path: StageAudibility,
    pub(crate) reflections: StageAudibility,
    pub(crate) quality: SourceQuality,
    pub(crate) physically_calibrated: Option<bool>,
    pub(crate) predicted_audibility_db: Option<f32>,
    pub(crate) path_eq: Option<[f32; 3]>,
    pub(crate) path_sh_energy: Option<f32>,
}

/// Resolved inputs for one source, kept explicit so the decision stays pure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SourceAcousticInputs {
    pub(crate) source_probes: ProbeCoverage,
    pub(crate) listener_probes: ProbeCoverage,
    /// Source enable, mute, and the panel's solo resolution combined.
    pub(crate) audible_in_mix: bool,
    pub(crate) stage_gains: StageOutputGains,
}

impl SourceAcousticState {
    pub(crate) const UNKNOWN: Self = Self {
        source_probes: ProbeCoverage::Unknown,
        listener_probes: ProbeCoverage::Unknown,
        occlusion: None,
        direct: StageAudibility::Unknown,
        path: StageAudibility::Unknown,
        reflections: StageAudibility::Unknown,
        quality: SourceQuality::Unknown,
        physically_calibrated: None,
        predicted_audibility_db: None,
        path_eq: None,
        path_sh_energy: None,
    };

    pub(crate) fn evaluate(
        inputs: SourceAcousticInputs,
        telemetry: AcousticTelemetry,
        source_index: usize,
    ) -> Self {
        let quality = telemetry.quality(source_index);
        Self {
            source_probes: inputs.source_probes,
            listener_probes: inputs.listener_probes,
            occlusion: telemetry.occlusion(source_index),
            direct: stage_audibility(
                inputs.audible_in_mix,
                inputs.stage_gains.direct,
                match quality {
                    SourceQuality::Unknown => StageAudibility::Unknown,
                    quality if quality.is_virtualized() => StageAudibility::Silent,
                    _ => StageAudibility::Audible,
                },
            ),
            path: stage_audibility(
                inputs.audible_in_mix,
                inputs.stage_gains.pathing,
                path_audibility(
                    telemetry.baked_pathing(),
                    quality,
                    inputs.source_probes,
                    inputs.listener_probes,
                ),
            ),
            reflections: stage_audibility(
                inputs.audible_in_mix,
                inputs.stage_gains.reflections,
                match quality {
                    SourceQuality::Unknown => StageAudibility::Unknown,
                    SourceQuality::Ungoverned
                    | SourceQuality::Governed(SourceQualityLevel::Full) => StageAudibility::Audible,
                    SourceQuality::Governed(_) => StageAudibility::Silent,
                },
            ),
            quality,
            physically_calibrated: telemetry.physically_calibrated(source_index),
            predicted_audibility_db: telemetry.predicted_audibility_db(source_index),
            path_eq: telemetry.path_eq(source_index),
            path_sh_energy: telemetry.path_sh_energy(source_index),
        }
    }
}

/// The mix silences a stage regardless of what the backend would deliver.
fn stage_audibility(
    audible_in_mix: bool,
    stage_gain: f32,
    backend: StageAudibility,
) -> StageAudibility {
    if !audible_in_mix || stage_gain <= 0.0 {
        return StageAudibility::Silent;
    }
    backend
}

/// Baked pathing needs a probe at both endpoints. A source teleported into
/// mid-air leaves its probe volume and silently loses the path stage, which is
/// the exact confusion these badges exist to remove.
fn path_audibility(
    baked_pathing: Option<bool>,
    quality: SourceQuality,
    source_probes: ProbeCoverage,
    listener_probes: ProbeCoverage,
) -> StageAudibility {
    if baked_pathing == Some(false) || quality.is_virtualized() {
        return StageAudibility::Silent;
    }
    if source_probes == ProbeCoverage::Uncovered || listener_probes == ProbeCoverage::Uncovered {
        return StageAudibility::Silent;
    }
    if baked_pathing.is_none()
        || source_probes == ProbeCoverage::Unknown
        || listener_probes == ProbeCoverage::Unknown
    {
        return StageAudibility::Unknown;
    }
    StageAudibility::Audible
}

/// Colour intent for one badge, resolved to a palette entry by the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BadgeTone {
    Ok,
    Warn,
    Off,
    Unknown,
}

/// Probe coverage and direct occlusion as one monospace run.
pub(crate) fn probe_text(state: SourceAcousticState) -> String {
    format!(
        "probes src {} | lis {}   occl {}",
        coverage_word(state.source_probes),
        coverage_word(state.listener_probes),
        occlusion_word(state.occlusion),
    )
}

/// Warn only when a position is provably outside every probe.
pub(crate) fn probe_tone(state: SourceAcousticState) -> BadgeTone {
    let coverage = [state.source_probes, state.listener_probes];
    if coverage.contains(&ProbeCoverage::Uncovered) {
        BadgeTone::Warn
    } else if coverage.contains(&ProbeCoverage::Unknown) {
        BadgeTone::Unknown
    } else {
        BadgeTone::Ok
    }
}

/// Stage chips in render order, each carrying its state in the leading glyph so
/// the row stays readable without relying on colour alone.
pub(crate) fn stage_chips(state: SourceAcousticState) -> [(String, BadgeTone); 3] {
    [
        stage_chip("direct", state.direct),
        stage_chip("path", state.path),
        stage_chip("refl", state.reflections),
    ]
}

pub(crate) fn quality_text(state: SourceAcousticState) -> String {
    let quality = match state.quality {
        SourceQuality::Unknown => "quality —",
        SourceQuality::Ungoverned => "quality ungoverned",
        SourceQuality::Governed(SourceQualityLevel::Full) => "quality Full",
        SourceQuality::Governed(SourceQualityLevel::DirectOnly) => "quality DirectOnly",
        SourceQuality::Governed(SourceQualityLevel::Virtualized) => "quality Virtualized",
    };
    let calibration = match state.physically_calibrated {
        Some(true) => "calibrated",
        Some(false) => "UNCALIBRATED",
        None => "calibration —",
    };
    match state.predicted_audibility_db {
        Some(predicted) if state.physically_calibrated == Some(true) => {
            format!("{quality}   {calibration}   predicted {predicted:.1} dB SPL")
        }
        Some(predicted) => format!("{quality}   {calibration}   predicted {predicted:.1} dB"),
        None => format!("{quality}   {calibration}"),
    }
}

pub(crate) fn quality_tone(state: SourceAcousticState) -> BadgeTone {
    match (state.quality, state.physically_calibrated) {
        (SourceQuality::Unknown, _) => BadgeTone::Unknown,
        (_, Some(false)) | (SourceQuality::Governed(SourceQualityLevel::DirectOnly), _) => {
            BadgeTone::Warn
        }
        (SourceQuality::Governed(SourceQualityLevel::Virtualized), _) => BadgeTone::Off,
        _ => BadgeTone::Ok,
    }
}

pub(crate) fn path_diagnostics_text(state: SourceAcousticState) -> String {
    match (state.path_sh_energy, state.path_eq) {
        (Some(energy), Some(eq)) => format!(
            "path SH {energy:.3e}   EQ {:.3}/{:.3}/{:.3}",
            eq[0], eq[1], eq[2]
        ),
        _ => "path SH —   EQ —/—/—".to_owned(),
    }
}

fn stage_chip(name: &str, audibility: StageAudibility) -> (String, BadgeTone) {
    match audibility {
        StageAudibility::Audible => (format!("+{name}"), BadgeTone::Ok),
        StageAudibility::Silent => (format!("-{name}"), BadgeTone::Off),
        StageAudibility::Unknown => (format!("?{name}"), BadgeTone::Unknown),
    }
}

fn coverage_word(coverage: ProbeCoverage) -> &'static str {
    match coverage {
        ProbeCoverage::Covered => "ok",
        ProbeCoverage::Uncovered => "none",
        ProbeCoverage::Unknown => "\u{2014}",
    }
}

fn occlusion_word(occlusion: Option<f32>) -> String {
    match occlusion {
        Some(occlusion) => format!("{occlusion:.2}"),
        None => "\u{2014}".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> SourceAcousticInputs {
        SourceAcousticInputs {
            source_probes: ProbeCoverage::Covered,
            listener_probes: ProbeCoverage::Covered,
            audible_in_mix: true,
            stage_gains: StageOutputGains::UNITY,
        }
    }

    fn telemetry() -> AcousticTelemetry {
        AcousticTelemetry {
            known: true,
            baked_pathing: true,
            governor_available: true,
            source_physically_calibrated: [true; MAX_ACTIVE_SOURCES],
            ..AcousticTelemetry::UNKNOWN
        }
    }

    #[test]
    fn unwired_probe_query_reports_unknown_for_every_position() {
        let query = ProbeCoverageQuery::unavailable();

        assert_eq!(
            query.coverage(EnuVector3::new(0.0, 0.0, 1.5)),
            ProbeCoverage::Unknown
        );
        assert_eq!(
            query.coverage(EnuVector3::new(120.0, -80.0, 63.0)),
            ProbeCoverage::Unknown
        );
    }

    #[test]
    fn sphere_backed_query_covers_the_street_and_leaves_a_lifted_source_outside() {
        let query = ProbeCoverageQuery::from_spheres(vec![
            (fightbox_steam_audio::EnuVector3::new(0.0, 0.0, 1.5), 2.0),
            (fightbox_steam_audio::EnuVector3::new(8.0, 0.0, 1.5), 2.0),
        ]);

        assert_eq!(
            query.coverage(EnuVector3::new(1.0, 0.0, 1.5)),
            ProbeCoverage::Covered
        );
        assert_eq!(
            query.coverage(EnuVector3::new(8.0, 0.0, 3.5)),
            ProbeCoverage::Covered
        );
        assert_eq!(
            query.coverage(EnuVector3::new(0.0, 0.0, 63.0)),
            ProbeCoverage::Uncovered
        );
        assert_eq!(
            query.coverage(EnuVector3::new(4.0, 0.0, 1.5)),
            ProbeCoverage::Uncovered
        );
    }

    #[test]
    fn wired_probe_query_separates_covered_and_uncovered_positions() {
        let query = ProbeCoverageQuery::from_fn(|position| position.up_m < 3.0);

        assert_eq!(
            query.coverage(EnuVector3::new(4.0, 4.0, 1.5)),
            ProbeCoverage::Covered
        );
        assert_eq!(
            query.coverage(EnuVector3::new(4.0, 4.0, 63.0)),
            ProbeCoverage::Uncovered
        );
    }

    #[test]
    fn unknown_state_renders_em_dashes_and_question_prefixed_chips() {
        let state = SourceAcousticState::UNKNOWN;

        assert_eq!(probe_text(state), "probes src — | lis —   occl —");
        assert_eq!(probe_tone(state), BadgeTone::Unknown);
        assert_eq!(
            stage_chips(state).map(|(text, _)| text),
            ["?direct", "?path", "?refl"]
        );
    }

    #[test]
    fn uncovered_source_probe_reads_none_and_warns() {
        let state = SourceAcousticState {
            source_probes: ProbeCoverage::Uncovered,
            listener_probes: ProbeCoverage::Covered,
            occlusion: Some(0.4249),
            ..SourceAcousticState::UNKNOWN
        };

        assert_eq!(probe_text(state), "probes src none | lis ok   occl 0.42");
        assert_eq!(probe_tone(state), BadgeTone::Warn);
    }

    #[test]
    fn a_source_lifted_out_of_probe_coverage_loses_only_the_path_stage() {
        let state = SourceAcousticState::evaluate(
            SourceAcousticInputs {
                source_probes: ProbeCoverage::Uncovered,
                ..inputs()
            },
            telemetry(),
            0,
        );

        assert_eq!(state.direct, StageAudibility::Audible);
        assert_eq!(state.path, StageAudibility::Silent);
        assert_eq!(state.reflections, StageAudibility::Audible);
        assert_eq!(
            stage_chips(state).map(|(text, tone)| (text, tone)),
            [
                ("+direct".to_owned(), BadgeTone::Ok),
                ("-path".to_owned(), BadgeTone::Off),
                ("+refl".to_owned(), BadgeTone::Ok),
            ]
        );
    }

    #[test]
    fn unknown_probe_coverage_leaves_path_unknown_rather_than_claiming_audible() {
        let state = SourceAcousticState::evaluate(
            SourceAcousticInputs {
                source_probes: ProbeCoverage::Unknown,
                listener_probes: ProbeCoverage::Unknown,
                ..inputs()
            },
            telemetry(),
            0,
        );

        assert_eq!(state.path, StageAudibility::Unknown);
        assert_eq!(state.direct, StageAudibility::Audible);
    }

    #[test]
    fn an_unbaked_generation_silences_pathing_even_with_probe_coverage() {
        let state = SourceAcousticState::evaluate(
            inputs(),
            AcousticTelemetry {
                baked_pathing: false,
                ..telemetry()
            },
            0,
        );

        assert_eq!(state.path, StageAudibility::Silent);
    }

    #[test]
    fn governor_rungs_drive_the_reflection_and_direct_chips() {
        let mut source_quality = [SourceQualityLevel::Full; MAX_ACTIVE_SOURCES];
        source_quality[1] = SourceQualityLevel::DirectOnly;
        source_quality[2] = SourceQualityLevel::Virtualized;
        let telemetry = AcousticTelemetry {
            source_quality,
            ..telemetry()
        };

        let full = SourceAcousticState::evaluate(inputs(), telemetry, 0);
        assert_eq!(full.reflections, StageAudibility::Audible);

        let direct_only = SourceAcousticState::evaluate(inputs(), telemetry, 1);
        assert_eq!(direct_only.direct, StageAudibility::Audible);
        assert_eq!(direct_only.path, StageAudibility::Audible);
        assert_eq!(direct_only.reflections, StageAudibility::Silent);

        let virtualized = SourceAcousticState::evaluate(inputs(), telemetry, 2);
        assert_eq!(virtualized.direct, StageAudibility::Silent);
        assert_eq!(virtualized.path, StageAudibility::Silent);
        assert_eq!(virtualized.reflections, StageAudibility::Silent);
    }

    #[test]
    fn source_status_reports_quality_calibration_and_path_proof_values() {
        let mut source_predicted_audibility_db = [0.0; MAX_ACTIVE_SOURCES];
        source_predicted_audibility_db[0] = 92.25;
        let mut source_path_eq = [None; MAX_ACTIVE_SOURCES];
        source_path_eq[0] = Some([0.626, 0.272, 0.151]);
        let mut source_path_sh_energy = [None; MAX_ACTIVE_SOURCES];
        source_path_sh_energy[0] = Some(1.25e-5);
        let state = SourceAcousticState::evaluate(
            inputs(),
            AcousticTelemetry {
                source_predicted_audibility_db,
                source_path_eq,
                source_path_sh_energy,
                ..telemetry()
            },
            0,
        );

        assert_eq!(
            quality_text(state),
            "quality Full   calibrated   predicted 92.2 dB SPL"
        );
        assert_eq!(quality_tone(state), BadgeTone::Ok);
        assert_eq!(
            path_diagnostics_text(state),
            "path SH 1.250e-5   EQ 0.626/0.272/0.151"
        );
    }

    #[test]
    fn an_ungoverned_generation_keeps_direct_and_reflections_running() {
        let state = SourceAcousticState::evaluate(
            inputs(),
            AcousticTelemetry {
                known: true,
                ..AcousticTelemetry::UNKNOWN
            },
            0,
        );

        assert_eq!(state.direct, StageAudibility::Audible);
        assert_eq!(state.reflections, StageAudibility::Audible);
        assert_eq!(state.path, StageAudibility::Silent);
    }

    #[test]
    fn telemetry_before_the_first_simulation_pass_reports_every_stage_unknown() {
        let state = SourceAcousticState::evaluate(inputs(), AcousticTelemetry::UNKNOWN, 0);

        assert_eq!(state.direct, StageAudibility::Unknown);
        assert_eq!(state.path, StageAudibility::Unknown);
        assert_eq!(state.reflections, StageAudibility::Unknown);
        assert_eq!(state.occlusion, None);
    }

    #[test]
    fn a_bypassed_stage_or_a_muted_source_silences_the_chips_it_should() {
        let bypassed = SourceAcousticState::evaluate(
            SourceAcousticInputs {
                stage_gains: StageOutputGains {
                    direct: 1.0,
                    pathing: 0.0,
                    reflections: 1.0,
                },
                ..inputs()
            },
            telemetry(),
            0,
        );
        assert_eq!(bypassed.path, StageAudibility::Silent);
        assert_eq!(bypassed.direct, StageAudibility::Audible);

        let muted = SourceAcousticState::evaluate(
            SourceAcousticInputs {
                audible_in_mix: false,
                ..inputs()
            },
            telemetry(),
            0,
        );
        assert_eq!(
            [muted.direct, muted.path, muted.reflections],
            [StageAudibility::Silent; 3]
        );
    }
}
