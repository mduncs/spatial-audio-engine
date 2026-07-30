use std::collections::BTreeSet;
use std::time::Instant;

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke};
use fightbox_api::{
    EngineConfig, EnuVector3, ExtentDescriptor, ListenerState, Pose, ReferenceLevel,
    SceneCalibration, SourceId, SourceProfile,
};
use fightbox_runtime::backend::{SimulationUpdate, SourceMotion};
use fightbox_runtime::{
    BlockProcessor, ProcessBlock, PropagationSnapshot, RenderError, RuntimeGraph,
    SimulationCadences, SimulationWorker, SnapshotPublication, SnapshotReader, SnapshotWriter,
    SourcePropagation,
};
use fightbox_steam_audio::{AudioConfig, MultiSourceDescriptor, build_multi_source_session};
use fightbox_world::{AcousticMesh, read_package};

use crate::LaunchArgs;
use crate::asset::load_asset;
use crate::fixture::{Fixture, Trajectory, load_baked, scene_mesh};
use crate::pose::{ListenerControl, PoseMailbox};

const BLOCK_SIZE: u32 = 128;
const SAMPLE_RATE: u32 = 48_000;
const YAW_RADIANS_PER_POINT: f32 = 0.008;
const DEFAULT_LISTEN_GAIN_DB: f32 = 50.0;
const DEFAULT_AUTOPILOT_SPEED_MPS: f32 = 6.0;
const METER_WINDOW_SECONDS: f32 = 0.5;
const FIRST_PERSON_VERTICAL_FOV_RADIANS: f32 = 70.0_f32.to_radians();
const FIRST_PERSON_NEAR_M: f32 = 0.1;

pub struct Workbench {
    mesh: AcousticMesh,
    edges: Vec<[usize; 2]>,
    sources: Vec<SourceView>,
    listener: ListenerControl,
    pose_mailbox: PoseMailbox,
    simulation: SimulationWorker,
    source_motion: [SourceMotion; fightbox_runtime::MAX_ACTIVE_SOURCES],
    audio: AudioState,
    camera: Camera,
    listen_gain_db: f32,
    listen_gain_writer: SnapshotWriter<f32>,
    meter_reader: SnapshotReader<MeterReading>,
    source_mix_writer: SnapshotWriter<SourceMix>,
    audio_block_reader: SnapshotReader<u64>,
    autopilot: Autopilot,
    startup_started: Instant,
    reflection_warmup_started: Instant,
    reflection_warmup_reported: bool,
    first_frame_reported: bool,
}

struct SourceView {
    id: String,
    position: EnuVector3,
    enabled: bool,
    muted: bool,
    soloed: bool,
    trajectory: Option<SourceTrajectory>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceMix {
    enabled: [bool; fightbox_runtime::MAX_ACTIVE_SOURCES],
    muted: [bool; fightbox_runtime::MAX_ACTIVE_SOURCES],
    soloed: [bool; fightbox_runtime::MAX_ACTIVE_SOURCES],
}

impl SourceMix {
    const ALL_AUDIBLE: Self = Self {
        enabled: [true; fightbox_runtime::MAX_ACTIVE_SOURCES],
        muted: [false; fightbox_runtime::MAX_ACTIVE_SOURCES],
        soloed: [false; fightbox_runtime::MAX_ACTIVE_SOURCES],
    };

    fn from_sources(sources: &[SourceView]) -> Self {
        let mut mix = Self::ALL_AUDIBLE;
        for (index, source) in sources.iter().enumerate() {
            mix.enabled[index] = source.enabled;
            mix.muted[index] = source.muted;
            mix.soloed[index] = source.soloed;
        }
        mix
    }

    #[cfg(any(feature = "live-output", test))]
    fn gains(self, source_count: usize) -> [f32; fightbox_runtime::MAX_ACTIVE_SOURCES] {
        let any_soloed = self.enabled[..source_count]
            .iter()
            .zip(&self.soloed[..source_count])
            .any(|(enabled, soloed)| *enabled && *soloed);
        std::array::from_fn(|index| {
            f32::from(
                index < source_count
                    && self.enabled[index]
                    && !self.muted[index]
                    && (!any_soloed || self.soloed[index]),
            )
        })
    }
}

enum AudioState {
    #[cfg(feature = "live-output")]
    Live(fightbox_runtime::live::LiveOutput),
    Unavailable(String),
}

impl Workbench {
    pub fn load(args: LaunchArgs, startup_started: Instant) -> Result<Self, String> {
        let phase_started = Instant::now();
        let package = read_package(&args.package)
            .map_err(|error| format!("cannot load package {}: {error}", args.package.display()))?;
        eprintln!(
            "[startup] package load: {} ms",
            phase_started.elapsed().as_millis()
        );

        let phase_started = Instant::now();
        let fixture = Fixture::read(&args.fixture)?;
        eprintln!(
            "[startup] fixture load: {} ms",
            phase_started.elapsed().as_millis()
        );

        let phase_started = Instant::now();
        let baked = load_baked(&args.baked, &package)?;
        eprintln!(
            "[startup] baked probes load: {} ms",
            phase_started.elapsed().as_millis()
        );

        let phase_started = Instant::now();
        let scene = scene_mesh(&package)?;
        eprintln!(
            "[startup] scene mesh preparation: {} ms",
            phase_started.elapsed().as_millis()
        );
        let listener = ListenerControl::at(
            fixture.initial_listener_position()?,
            to_enu(fixture.listener.forward_enu),
        );
        let initial_listener = listener.listener_state(EnuVector3::default());
        let (pose_mailbox, pose_reader) = PoseMailbox::new(initial_listener);

        let mut prepared_sources = Vec::with_capacity(fixture.sources.len());
        let mut source_motion = [SourceMotion::default(); fightbox_runtime::MAX_ACTIVE_SOURCES];
        let mut source_views = Vec::with_capacity(fixture.sources.len());
        for (index, source) in fixture.sources.iter().enumerate() {
            let position = source.initial_position()?;
            let trajectory = source
                .trajectory
                .as_ref()
                .map(SourceTrajectory::from_fixture)
                .transpose()?;
            let asset = load_asset(&source.asset_id)?;
            let pose = Pose {
                position,
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            };
            prepared_sources.push((
                SourceProfile {
                    id: SourceId::new(&source.id),
                    pose,
                    reference_level: ReferenceLevel::SplAtOneMeter {
                        db_spl: source.reference_level.db_spl as f32,
                    },
                    asset_analysis: asset.analysis,
                    extent: ExtentDescriptor::Point,
                    max_speed_mps: source
                        .trajectory
                        .as_ref()
                        .map(|trajectory| {
                            trajectory.max_speed_mps.unwrap_or(trajectory.speed_mps) as f32
                        })
                        .unwrap_or(0.0),
                },
                asset.samples,
            ));
            source_motion[index] = SourceMotion {
                active: true,
                pose,
                linear_velocity_mps: EnuVector3::default(),
            };
            source_views.push(SourceView {
                id: source.id.clone(),
                position,
                enabled: source.default_enabled,
                muted: false,
                soloed: false,
                trajectory,
            });
        }
        let descriptors = source_views
            .iter()
            .map(|source| MultiSourceDescriptor::at(source.position))
            .collect::<Vec<_>>();
        let audio_config = AudioConfig {
            sample_rate_hz: SAMPLE_RATE as i32,
            frame_size: BLOCK_SIZE as i32,
        };
        let phase_started = Instant::now();
        let (runner, backend) = build_multi_source_session(
            &scene,
            &baked,
            audio_config,
            fixture.simulation_config(),
            &descriptors,
        )
        .map_err(|error| format!("cannot build Steam Audio session: {error}"))?;
        eprintln!(
            "[startup] steam scene + simulator build: {} ms",
            phase_started.elapsed().as_millis()
        );
        let initial_update = SimulationUpdate {
            listener: initial_listener,
            sources: source_motion,
        };
        let reflection_warmup_started = Instant::now();
        let simulation = SimulationWorker::new(
            Box::new(runner),
            initial_update,
            SimulationCadences::default(),
        )
        .map_err(|error| format!("cannot start simulation worker: {error:?}"))?;
        eprintln!(
            "[startup] simulation worker started: {} ms",
            reflection_warmup_started.elapsed().as_millis()
        );

        let phase_started = Instant::now();
        let propagation = PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: u64::MAX,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index < prepared_sources.len(),
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        };
        let (_writer, reader) = SnapshotPublication::new(propagation);
        let engine_config = EngineConfig {
            sample_rate_hz: SAMPLE_RATE,
            block_size_frames: BLOCK_SIZE,
            max_active_sources: prepared_sources.len() as u8,
            ..EngineConfig::default()
        };
        let mut graph = RuntimeGraph::new_with_backend(engine_config, reader, Box::new(backend))
            .map_err(|error| format!("cannot create runtime graph: {error:?}"))?;
        graph.set_listener_state(initial_listener);
        for (index, (profile, _)) in prepared_sources.iter().enumerate() {
            graph
                .set_source(index, profile, SceneCalibration::default())
                .map_err(|error| format!("cannot configure source {index}: {error:?}"))?;
        }
        eprintln!(
            "[startup] runtime graph configuration: {} ms",
            phase_started.elapsed().as_millis()
        );
        let (listen_gain_writer, listen_gain_reader) =
            SnapshotPublication::new(DEFAULT_LISTEN_GAIN_DB);
        let (meter_writer, meter_reader) = SnapshotPublication::new(MeterReading::SILENT);
        let initial_source_mix = SourceMix::from_sources(&source_views);
        let (source_mix_writer, source_mix_reader) = SnapshotPublication::new(initial_source_mix);
        let (audio_block_writer, audio_block_reader) = SnapshotPublication::new(0_u64);
        let processor = LateBoundProcessor::new(
            graph,
            pose_reader,
            listen_gain_reader,
            meter_writer,
            MeterAccumulator::new(SAMPLE_RATE, BLOCK_SIZE, METER_WINDOW_SECONDS),
            audio_block_writer,
        );
        let signals = prepared_sources
            .into_iter()
            .map(|(_, samples)| samples)
            .collect();
        let phase_started = Instant::now();
        let audio = start_audio(
            processor,
            engine_config,
            signals,
            source_mix_reader,
            args.device.as_deref(),
        );
        eprintln!(
            "[startup] audio stream initialization: {} ms",
            phase_started.elapsed().as_millis()
        );
        let phase_started = Instant::now();
        let edges = mesh_edges(&package.mesh);
        let camera = Camera::for_mesh(&package.mesh);
        let autopilot = Autopilot::new(Bounds2::for_mesh(&package.mesh));
        eprintln!(
            "[startup] workbench view preparation: {} ms",
            phase_started.elapsed().as_millis()
        );
        Ok(Self {
            mesh: package.mesh,
            edges,
            sources: source_views,
            listener,
            pose_mailbox,
            simulation,
            source_motion,
            audio,
            camera,
            listen_gain_db: DEFAULT_LISTEN_GAIN_DB,
            listen_gain_writer,
            meter_reader,
            source_mix_writer,
            audio_block_reader,
            autopilot,
            startup_started,
            reflection_warmup_started,
            reflection_warmup_reported: false,
            first_frame_reported: false,
        })
    }

    fn update_source_motion(&mut self) {
        let elapsed_blocks = self.audio_block_reader.read();
        for (index, source) in self.sources.iter_mut().enumerate() {
            let Some(trajectory) = &source.trajectory else {
                continue;
            };
            let sample = trajectory.sample_at_block(elapsed_blocks);
            source.position = sample.position;
            self.source_motion[index].pose.position = sample.position;
            self.source_motion[index].pose.forward = sample.direction;
            self.source_motion[index].linear_velocity_mps =
                scale(sample.direction, trajectory.speed_mps);
        }
    }

    fn update_control(&mut self, ctx: &egui::Context, drag_delta_x: f32) {
        if drag_delta_x != 0.0 && !self.autopilot.enabled {
            self.listener.turn(drag_delta_x * YAW_RADIANS_PER_POINT);
        }
        let (forward, right, sprinting, delta_seconds) = ctx.input(|input| {
            (
                axis(input, egui::Key::W, egui::Key::S),
                axis(input, egui::Key::D, egui::Key::A),
                input.modifiers.shift,
                input.stable_dt.min(0.1),
            )
        });
        if self.autopilot.enabled && (forward != 0.0 || right != 0.0) {
            self.autopilot.enabled = false;
        }
        let velocity = if self.autopilot.enabled {
            let sample = self.autopilot.advance(delta_seconds);
            self.listener.position = EnuVector3::new(
                sample.position[0],
                sample.position[1],
                self.listener.position.up_m,
            );
            self.listener.yaw_radians = sample.direction[0].atan2(sample.direction[1]);
            EnuVector3::new(
                sample.direction[0] * self.autopilot.speed_mps,
                sample.direction[1] * self.autopilot.speed_mps,
                0.0,
            )
        } else {
            self.listener.walk(forward, right, sprinting, delta_seconds)
        };
        let listener = self.listener.listener_state(velocity);
        self.pose_mailbox.publish(listener);
        self.simulation.publish_update(SimulationUpdate {
            listener,
            sources: self.source_motion,
        });
    }

    fn draw_scene(&self, painter: &egui::Painter, rect: Rect) {
        painter.rect_filled(rect, 0.0, Color32::from_rgb(13, 18, 24));
        for edge in &self.edges {
            let a = self.mesh.vertices_enu_m[edge[0]];
            let b = self.mesh.vertices_enu_m[edge[1]];
            if let (Some(a), Some(b)) = (self.camera.project(a, rect), self.camera.project(b, rect))
            {
                painter.line_segment([a, b], Stroke::new(1.0, Color32::from_rgb(82, 109, 126)));
            }
        }
        for source in &self.sources {
            if let Some(point) = self.camera.project(source.position, rect) {
                painter.circle_filled(point, 5.0, Color32::from_rgb(255, 174, 66));
                painter.text(
                    point + egui::vec2(8.0, -8.0),
                    egui::Align2::LEFT_BOTTOM,
                    &source.id,
                    egui::FontId::monospace(11.0),
                    Color32::from_rgb(255, 213, 146),
                );
            }
        }
        let listener = self.listener.position;
        let arrow_end = add(listener, scale(self.listener.forward(), 4.0));
        if let (Some(origin), Some(end)) = (
            self.camera.project(listener, rect),
            self.camera.project(arrow_end, rect),
        ) {
            painter.circle_filled(origin, 5.0, Color32::from_rgb(64, 211, 176));
            painter.arrow(
                origin,
                end - origin,
                Stroke::new(2.5, Color32::from_rgb(64, 211, 176)),
            );
        }
    }

    fn draw_first_person(&self, painter: &egui::Painter, rect: Rect) {
        painter.rect_filled(rect, 3.0, Color32::from_rgb(8, 12, 17));
        painter.rect_stroke(
            rect,
            3.0,
            Stroke::new(1.0, Color32::from_rgb(105, 136, 153)),
            egui::StrokeKind::Inside,
        );
        let projection = FirstPersonProjection::new(
            self.listener.position,
            self.listener.yaw_radians,
            FIRST_PERSON_VERTICAL_FOV_RADIANS,
            FIRST_PERSON_NEAR_M,
        );
        for edge in &self.edges {
            let a = self.mesh.vertices_enu_m[edge[0]];
            let b = self.mesh.vertices_enu_m[edge[1]];
            if let Some([a, b]) = projection.project_segment(a, b, rect) {
                painter.line_segment([a, b], Stroke::new(1.0, Color32::from_rgb(76, 110, 130)));
            }
        }
        for source in &self.sources {
            if let Some((point, distance)) = projection.project_point(source.position, rect) {
                let radius = (32.0 / distance.max(1.0)).clamp(2.5, 10.0);
                painter.circle_filled(point, radius, Color32::from_rgb(255, 174, 66));
                painter.text(
                    point + egui::vec2(radius + 3.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &source.id,
                    egui::FontId::monospace(10.0),
                    Color32::from_rgb(255, 213, 146),
                );
            }
        }
        painter.text(
            rect.left_top() + egui::vec2(8.0, 7.0),
            egui::Align2::LEFT_TOP,
            "LISTENER VIEW",
            egui::FontId::monospace(10.0),
            Color32::from_rgb(142, 173, 188),
        );
    }

    fn perf_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Fightbox");
        ui.label("WASD walk · Shift sprint");
        ui.label("Drag in view to turn head");
        ui.separator();
        ui.monospace(format!(
            "ENU  {:7.2}  {:7.2}  {:5.2} m",
            self.listener.position.east_m,
            self.listener.position.north_m,
            self.listener.position.up_m
        ));
        ui.monospace(format!(
            "yaw  {:6.1}°",
            self.listener.yaw_radians.to_degrees()
        ));
        ui.separator();
        ui.heading("Listen");
        if ui
            .add(
                egui::Slider::new(&mut self.listen_gain_db, 0.0..=70.0)
                    .suffix(" dB")
                    .text("makeup"),
            )
            .changed()
        {
            self.listen_gain_writer.publish(self.listen_gain_db);
        }
        let meter = self.meter_reader.read();
        ui.monospace(format!("peak  {:7.1} dBFS", meter.peak_dbfs));
        ui.monospace(format!("RMS   {:7.1} dBFS", meter.rms_dbfs));
        ui.separator();
        ui.heading("Sources");
        let mut source_mix_changed = false;
        for source in &mut self.sources {
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut source.enabled, "")
                    .on_hover_text("Enable this source")
                    .changed()
                {
                    source_mix_changed = true;
                }
                if ui
                    .selectable_label(source.muted, "M")
                    .on_hover_text("Mute this source")
                    .clicked()
                {
                    source.muted = !source.muted;
                    source_mix_changed = true;
                }
                if ui
                    .selectable_label(source.soloed, "S")
                    .on_hover_text("Solo this source")
                    .clicked()
                {
                    source.soloed = !source.soloed;
                    source_mix_changed = true;
                }
                ui.monospace(&source.id);
            });
        }
        if source_mix_changed {
            self.source_mix_writer
                .publish(SourceMix::from_sources(&self.sources));
        }
        ui.separator();
        ui.heading("Autopilot");
        let was_enabled = self.autopilot.enabled;
        ui.checkbox(&mut self.autopilot.enabled, "follow city circuit");
        if self.autopilot.enabled && !was_enabled {
            self.autopilot.reset();
        }
        ui.add(
            egui::Slider::new(&mut self.autopilot.speed_mps, 1.0..=30.0)
                .suffix(" m/s")
                .text("speed"),
        );
        ui.separator();
        ui.heading("Audio callback");
        match &self.audio {
            #[cfg(feature = "live-output")]
            AudioState::Live(output) => {
                let telemetry = output.telemetry();
                ui.monospace(format!(
                    "window p99    {:6.3} ms",
                    telemetry.callback_timings.p99_ms
                ));
                ui.monospace(format!(
                    "window p99.9  {:6.3} ms",
                    telemetry.callback_timings.p99_9_ms
                ));
                ui.monospace(format!(
                    "run p99       {:6.3} ms",
                    telemetry.run_callback_timings.p99_ms
                ));
                ui.monospace(format!(
                    "run p99.9     {:6.3} ms",
                    telemetry.run_callback_timings.p99_9_ms
                ));
                ui.separator();
                ui.monospace(format!("callbacks      {}", telemetry.callback_count));
                ui.monospace(format!("deadline miss  {}", telemetry.deadline_misses));
                ui.monospace(format!("process error  {}", telemetry.processing_errors));
                ui.monospace(format!("stream error   {}", telemetry.stream_errors));
                fault_rows(ui, telemetry.faults);
            }
            AudioState::Unavailable(message) => {
                ui.colored_label(Color32::from_rgb(255, 172, 90), "Audio unavailable");
                ui.label(message);
            }
        }
        let simulation = self.simulation.telemetry();
        ui.separator();
        ui.heading("Simulation");
        ui.monospace(format!(
            "failures d/p/r  {}/{}/{}",
            simulation.direct.failures,
            simulation.pathing.failures,
            simulation.reflections.failures
        ));
    }
}

impl eframe::App for Workbench {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_source_motion();
        egui::SidePanel::right("performance")
            .resizable(false)
            .default_width(270.0)
            .show(ctx, |ui| self.perf_panel(ui));
        let mut drag_delta_x = 0.0;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let (response, painter) =
                    ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
                if response.dragged_by(egui::PointerButton::Primary) {
                    drag_delta_x = ui.input(|input| input.pointer.delta().x);
                }
                self.draw_scene(&painter, response.rect);
                let pip_size = egui::vec2(
                    (response.rect.width() * 0.32).max(260.0),
                    (response.rect.height() * 0.30).max(170.0),
                );
                let pip_rect = Rect::from_min_size(
                    response.rect.right_bottom() - pip_size - egui::vec2(14.0, 14.0),
                    pip_size,
                );
                self.draw_first_person(&painter, pip_rect);
            });
        self.update_control(ctx, drag_delta_x);
        ctx.request_repaint();
        if !self.reflection_warmup_reported {
            let telemetry = self.simulation.telemetry();
            if let Some(pass_ns) = telemetry.reflections.timings.newest_ns() {
                eprintln!(
                    "[startup] reflection warmup: {} ms (pass {} ms)",
                    self.reflection_warmup_started.elapsed().as_millis(),
                    pass_ns / 1_000_000
                );
                self.reflection_warmup_reported = true;
            }
        }
        if !self.first_frame_reported {
            eprintln!(
                "[startup] total to first frame: {} ms",
                self.startup_started.elapsed().as_millis()
            );
            self.first_frame_reported = true;
        }
    }
}

#[cfg(feature = "live-output")]
fn fault_rows(ui: &mut egui::Ui, faults: fightbox_runtime::FaultCounters) {
    ui.monospace(format!("snapshot stale {}", faults.snapshot_stale));
    ui.monospace(format!("graph deadline {}", faults.deadline_miss));
    ui.monospace(format!("backend error  {}", faults.backend_render_error));
}

fn axis(input: &egui::InputState, positive: egui::Key, negative: egui::Key) -> f32 {
    f32::from(input.key_down(positive)) - f32::from(input.key_down(negative))
}

trait ListenerStateSink {
    fn set_listener_state(&mut self, listener: ListenerState);
}

impl ListenerStateSink for RuntimeGraph {
    fn set_listener_state(&mut self, listener: ListenerState) {
        RuntimeGraph::set_listener_state(self, listener);
    }
}

struct LateBoundProcessor<P> {
    processor: P,
    pose_reader: SnapshotReader<ListenerState>,
    listen_gain_reader: SnapshotReader<f32>,
    meter_writer: SnapshotWriter<MeterReading>,
    meter: MeterAccumulator,
    audio_block_writer: SnapshotWriter<u64>,
    elapsed_blocks: u64,
}

impl<P> LateBoundProcessor<P> {
    fn new(
        processor: P,
        pose_reader: SnapshotReader<ListenerState>,
        listen_gain_reader: SnapshotReader<f32>,
        meter_writer: SnapshotWriter<MeterReading>,
        meter: MeterAccumulator,
        audio_block_writer: SnapshotWriter<u64>,
    ) -> Self {
        Self {
            processor,
            pose_reader,
            listen_gain_reader,
            meter_writer,
            meter,
            audio_block_writer,
            elapsed_blocks: 0,
        }
    }
}

impl<P: BlockProcessor + ListenerStateSink> BlockProcessor for LateBoundProcessor<P> {
    fn block_size_frames(&self) -> usize {
        self.processor.block_size_frames()
    }

    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
        let listener = self.pose_reader.read();
        self.processor.set_listener_state(listener);
        let ProcessBlock {
            now_ns,
            sources,
            output_left,
            output_right,
        } = block;
        self.processor.process_block(ProcessBlock {
            now_ns,
            sources,
            output_left: &mut *output_left,
            output_right: &mut *output_right,
        })?;
        let gain = db_to_linear(self.listen_gain_reader.read());
        apply_output_gain(output_left, output_right, gain);
        let reading = self.meter.observe(output_left, output_right);
        self.meter_writer.publish(reading);
        self.elapsed_blocks = self.elapsed_blocks.saturating_add(1);
        self.audio_block_writer.publish(self.elapsed_blocks);
        Ok(())
    }

    fn fault_counters(&self) -> fightbox_runtime::FaultCounters {
        self.processor.fault_counters()
    }
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn apply_output_gain(left: &mut [f32], right: &mut [f32], gain: f32) {
    for sample in left.iter_mut().chain(right) {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MeterReading {
    peak_dbfs: f32,
    rms_dbfs: f32,
}

impl MeterReading {
    const SILENT: Self = Self {
        peak_dbfs: -120.0,
        rms_dbfs: -120.0,
    };
}

#[derive(Clone, Copy, Debug, Default)]
struct MeterBlock {
    peak: f32,
    square_sum: f64,
    samples: usize,
}

struct MeterAccumulator {
    blocks: Vec<MeterBlock>,
    next: usize,
    square_sum: f64,
    samples: usize,
}

impl MeterAccumulator {
    fn new(sample_rate: u32, block_size: u32, window_seconds: f32) -> Self {
        let block_count =
            ((sample_rate as f32 * window_seconds) / block_size as f32).ceil() as usize;
        Self {
            blocks: vec![MeterBlock::default(); block_count.max(1)],
            next: 0,
            square_sum: 0.0,
            samples: 0,
        }
    }

    fn observe(&mut self, left: &[f32], right: &[f32]) -> MeterReading {
        let outgoing = self.blocks[self.next];
        self.square_sum -= outgoing.square_sum;
        self.samples -= outgoing.samples;
        let mut incoming = MeterBlock::default();
        for sample in left.iter().chain(right) {
            incoming.peak = incoming.peak.max(sample.abs());
            incoming.square_sum += f64::from(*sample) * f64::from(*sample);
            incoming.samples += 1;
        }
        self.blocks[self.next] = incoming;
        self.next = (self.next + 1) % self.blocks.len();
        self.square_sum += incoming.square_sum;
        self.samples += incoming.samples;
        let peak = self
            .blocks
            .iter()
            .map(|block| block.peak)
            .fold(0.0_f32, f32::max);
        let rms = if self.samples == 0 {
            0.0
        } else {
            (self.square_sum / self.samples as f64).sqrt() as f32
        };
        MeterReading {
            peak_dbfs: amplitude_dbfs(peak),
            rms_dbfs: amplitude_dbfs(rms),
        }
    }
}

fn amplitude_dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        -120.0
    } else {
        (20.0 * amplitude.log10()).max(-120.0)
    }
}

#[cfg(feature = "live-output")]
struct LoopingInput {
    signals: Vec<Vec<f32>>,
    offsets: Vec<usize>,
    source_mix_reader: SnapshotReader<SourceMix>,
}

#[cfg(feature = "live-output")]
impl fightbox_runtime::live::LiveInputProvider for LoopingInput {
    fn fill_block(&mut self, sources: &mut fightbox_runtime::live::LiveSourceBuffer) {
        let gains = self.source_mix_reader.read().gains(self.signals.len());
        for index in 0..self.signals.len() {
            let Some(output) = sources.add_source(index) else {
                return;
            };
            let signal = &self.signals[index];
            let mut offset = self.offsets[index];
            for sample in output {
                *sample = signal[offset] * gains[index];
                offset = (offset + 1) % signal.len();
            }
            self.offsets[index] = offset;
        }
    }
}

#[cfg(feature = "live-output")]
fn start_audio<P: BlockProcessor + Send + 'static>(
    processor: P,
    config: EngineConfig,
    signals: Vec<Vec<f32>>,
    source_mix_reader: SnapshotReader<SourceMix>,
    device: Option<&str>,
) -> AudioState {
    let input = Box::new(LoopingInput {
        offsets: vec![0; signals.len()],
        signals,
        source_mix_reader,
    });
    let output = match device {
        Some(name) => {
            fightbox_runtime::live::LiveOutput::new_named_with_input(processor, config, name, input)
        }
        None => {
            fightbox_runtime::live::LiveOutput::new_default_with_input(processor, config, input)
        }
    };
    match output {
        Ok(output) => match output.start() {
            Ok(()) => AudioState::Live(output),
            Err(error) => AudioState::Unavailable(format!("cannot start output: {error:?}")),
        },
        Err(error) => AudioState::Unavailable(format!("cannot open output: {error:?}")),
    }
}

#[cfg(not(feature = "live-output"))]
fn start_audio<P: BlockProcessor + Send + 'static>(
    _processor: P,
    _config: EngineConfig,
    _signals: Vec<Vec<f32>>,
    _source_mix_reader: SnapshotReader<SourceMix>,
    _device: Option<&str>,
) -> AudioState {
    AudioState::Unavailable("binary was built without the live-output feature".into())
}

fn mesh_edges(mesh: &AcousticMesh) -> Vec<[usize; 2]> {
    let mut edges = BTreeSet::new();
    for triangle in &mesh.triangles {
        for [left, right] in [
            [triangle[0], triangle[1]],
            [triangle[1], triangle[2]],
            [triangle[2], triangle[0]],
        ] {
            let edge = if left <= right {
                [left as usize, right as usize]
            } else {
                [right as usize, left as usize]
            };
            edges.insert(edge);
        }
    }
    edges.into_iter().collect()
}

#[derive(Clone, Copy)]
struct Camera {
    eye: [f32; 3],
    target: [f32; 3],
}

impl Camera {
    fn for_mesh(mesh: &AcousticMesh) -> Self {
        let first = mesh.vertices_enu_m.first().copied().unwrap_or_default();
        let mut min = [first.east_m, first.north_m, first.up_m];
        let mut max = min;
        for vertex in &mesh.vertices_enu_m {
            let point = [vertex.east_m, vertex.north_m, vertex.up_m];
            for axis in 0..3 {
                min[axis] = min[axis].min(point[axis]);
                max[axis] = max[axis].max(point[axis]);
            }
        }
        let target = [
            (min[0] + max[0]) * 0.5,
            (min[1] + max[1]) * 0.5,
            (min[2] + max[2]) * 0.5,
        ];
        let radius = (max[0] - min[0])
            .max(max[1] - min[1])
            .max(max[2] - min[2])
            .max(10.0);
        Self {
            eye: [
                target[0] + radius * 0.85,
                target[1] - radius * 0.95,
                target[2] + radius * 0.75,
            ],
            target,
        }
    }

    fn project(self, point: EnuVector3, rect: Rect) -> Option<Pos2> {
        let forward = normalize3(sub3(self.target, self.eye));
        let right = normalize3(cross3(forward, [0.0, 0.0, 1.0]));
        let up = cross3(right, forward);
        let relative = sub3([point.east_m, point.north_m, point.up_m], self.eye);
        let depth = dot3(relative, forward);
        if depth <= 0.1 {
            return None;
        }
        let scale = rect.height().min(rect.width()) * 0.9 / depth;
        Some(Pos2::new(
            rect.center().x + dot3(relative, right) * scale,
            rect.center().y - dot3(relative, up) * scale,
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds2 {
    min: [f32; 2],
    max: [f32; 2],
}

impl Bounds2 {
    fn for_mesh(mesh: &AcousticMesh) -> Self {
        let first = mesh.vertices_enu_m.first().copied().unwrap_or_default();
        let mut bounds = Self {
            min: [first.east_m, first.north_m],
            max: [first.east_m, first.north_m],
        };
        for vertex in &mesh.vertices_enu_m {
            bounds.min[0] = bounds.min[0].min(vertex.east_m);
            bounds.min[1] = bounds.min[1].min(vertex.north_m);
            bounds.max[0] = bounds.max[0].max(vertex.east_m);
            bounds.max[1] = bounds.max[1].max(vertex.north_m);
        }
        bounds
    }

    fn inset_circuit(self) -> RectCircuit {
        let width = self.max[0] - self.min[0];
        let height = self.max[1] - self.min[1];
        // A proportional inset lands on the first interior street of regular
        // city grids while still producing a useful circuit for small scenes.
        let inset = width.min(height) * 0.16;
        RectCircuit {
            min: [self.min[0] + inset, self.min[1] + inset],
            max: [self.max[0] - inset, self.max[1] - inset],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RectCircuit {
    min: [f32; 2],
    max: [f32; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CircuitSample {
    position: [f32; 2],
    direction: [f32; 2],
}

impl RectCircuit {
    fn perimeter(self) -> f32 {
        2.0 * ((self.max[0] - self.min[0]) + (self.max[1] - self.min[1]))
    }

    fn sample(self, distance: f32) -> CircuitSample {
        let width = self.max[0] - self.min[0];
        let height = self.max[1] - self.min[1];
        let mut distance = distance.rem_euclid(self.perimeter());
        if distance < width {
            return CircuitSample {
                position: [self.min[0] + distance, self.min[1]],
                direction: [1.0, 0.0],
            };
        }
        distance -= width;
        if distance < height {
            return CircuitSample {
                position: [self.max[0], self.min[1] + distance],
                direction: [0.0, 1.0],
            };
        }
        distance -= height;
        if distance < width {
            return CircuitSample {
                position: [self.max[0] - distance, self.max[1]],
                direction: [-1.0, 0.0],
            };
        }
        distance -= width;
        CircuitSample {
            position: [self.min[0], self.max[1] - distance],
            direction: [0.0, -1.0],
        }
    }
}

struct Autopilot {
    enabled: bool,
    speed_mps: f32,
    distance_m: f32,
    circuit: RectCircuit,
}

impl Autopilot {
    fn new(bounds: Bounds2) -> Self {
        Self {
            enabled: false,
            speed_mps: DEFAULT_AUTOPILOT_SPEED_MPS,
            distance_m: 0.0,
            circuit: bounds.inset_circuit(),
        }
    }

    fn reset(&mut self) {
        self.distance_m = 0.0;
    }

    fn advance(&mut self, delta_seconds: f32) -> CircuitSample {
        self.distance_m =
            (self.distance_m + self.speed_mps * delta_seconds).rem_euclid(self.circuit.perimeter());
        self.circuit.sample(self.distance_m)
    }
}

#[derive(Clone, Copy)]
struct FirstPersonProjection {
    eye: EnuVector3,
    forward: [f32; 2],
    right: [f32; 2],
    tan_half_vertical_fov: f32,
    near_m: f32,
}

impl FirstPersonProjection {
    fn new(eye: EnuVector3, yaw_radians: f32, vertical_fov_radians: f32, near_m: f32) -> Self {
        Self {
            eye,
            forward: [yaw_radians.sin(), yaw_radians.cos()],
            right: [yaw_radians.cos(), -yaw_radians.sin()],
            tan_half_vertical_fov: (vertical_fov_radians * 0.5).tan(),
            near_m,
        }
    }

    fn camera_point(self, point: EnuVector3) -> [f32; 3] {
        let east = point.east_m - self.eye.east_m;
        let north = point.north_m - self.eye.north_m;
        [
            east * self.right[0] + north * self.right[1],
            point.up_m - self.eye.up_m,
            east * self.forward[0] + north * self.forward[1],
        ]
    }

    fn screen_point(self, point: [f32; 3], rect: Rect) -> Pos2 {
        let aspect = rect.width() / rect.height().max(1.0);
        let x = point[0] / (point[2] * self.tan_half_vertical_fov * aspect);
        let y = point[1] / (point[2] * self.tan_half_vertical_fov);
        Pos2::new(
            rect.center().x + x * rect.width() * 0.5,
            rect.center().y - y * rect.height() * 0.5,
        )
    }

    fn project_point(self, point: EnuVector3, rect: Rect) -> Option<(Pos2, f32)> {
        let camera = self.camera_point(point);
        (camera[2] >= self.near_m).then(|| {
            let distance = dot3(camera, camera).sqrt();
            (self.screen_point(camera, rect), distance)
        })
    }

    fn project_segment(self, a: EnuVector3, b: EnuVector3, rect: Rect) -> Option<[Pos2; 2]> {
        let mut a = self.camera_point(a);
        let mut b = self.camera_point(b);
        if a[2] < self.near_m && b[2] < self.near_m {
            return None;
        }
        if a[2] < self.near_m {
            a = clip_to_depth(a, b, self.near_m);
        } else if b[2] < self.near_m {
            b = clip_to_depth(b, a, self.near_m);
        }
        Some([self.screen_point(a, rect), self.screen_point(b, rect)])
    }
}

fn clip_to_depth(behind: [f32; 3], ahead: [f32; 3], depth: f32) -> [f32; 3] {
    let t = (depth - behind[2]) / (ahead[2] - behind[2]);
    [
        behind[0] + (ahead[0] - behind[0]) * t,
        behind[1] + (ahead[1] - behind[1]) * t,
        depth,
    ]
}

fn add(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m + right.east_m,
        left.north_m + right.north_m,
        left.up_m + right.up_m,
    )
}

fn scale(vector: EnuVector3, amount: f32) -> EnuVector3 {
    EnuVector3::new(
        vector.east_m * amount,
        vector.north_m * amount,
        vector.up_m * amount,
    )
}

#[derive(Clone, Debug)]
struct SourceTrajectory {
    waypoints: Vec<EnuVector3>,
    segment_lengths_m: Vec<f32>,
    cycle_length_m: f32,
    speed_mps: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SourceTrajectorySample {
    position: EnuVector3,
    direction: EnuVector3,
}

impl SourceTrajectory {
    fn from_fixture(trajectory: &Trajectory) -> Result<Self, String> {
        let waypoints = trajectory
            .waypoints_m
            .iter()
            .copied()
            .map(to_enu)
            .collect::<Vec<_>>();
        // Source paths are cyclic: after the final waypoint they travel along
        // the closing segment back to the first waypoint and repeat.
        let segment_lengths_m = (0..waypoints.len())
            .map(|index| {
                vector_length(subtract(
                    waypoints[(index + 1) % waypoints.len()],
                    waypoints[index],
                ))
            })
            .collect::<Vec<_>>();
        let cycle_length_m: f32 = segment_lengths_m.iter().sum();
        if !cycle_length_m.is_finite() || cycle_length_m <= 0.0 {
            return Err("source trajectory must contain a non-zero segment".into());
        }
        Ok(Self {
            waypoints,
            segment_lengths_m,
            cycle_length_m,
            speed_mps: trajectory.speed_mps as f32,
        })
    }

    fn sample_at_block(&self, elapsed_blocks: u64) -> SourceTrajectorySample {
        let elapsed_seconds =
            elapsed_blocks as f64 * f64::from(BLOCK_SIZE) / f64::from(SAMPLE_RATE);
        let mut distance_m = (elapsed_seconds * f64::from(self.speed_mps))
            .rem_euclid(f64::from(self.cycle_length_m)) as f32;
        for (index, segment_length_m) in self.segment_lengths_m.iter().copied().enumerate() {
            if segment_length_m == 0.0 {
                continue;
            }
            if distance_m < segment_length_m {
                let start = self.waypoints[index];
                let delta = subtract(self.waypoints[(index + 1) % self.waypoints.len()], start);
                let direction = scale(delta, 1.0 / segment_length_m);
                return SourceTrajectorySample {
                    position: add(start, scale(delta, distance_m / segment_length_m)),
                    direction,
                };
            }
            distance_m -= segment_length_m;
        }
        SourceTrajectorySample {
            position: self.waypoints[0],
            direction: EnuVector3::default(),
        }
    }
}

fn subtract(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m - right.east_m,
        left.north_m - right.north_m,
        left.up_m - right.up_m,
    )
}

fn vector_length(vector: EnuVector3) -> f32 {
    (vector.east_m * vector.east_m + vector.north_m * vector.north_m + vector.up_m * vector.up_m)
        .sqrt()
}

fn to_enu(value: [f64; 3]) -> EnuVector3 {
    EnuVector3::new(value[0] as f32, value[1] as f32, value[2] as f32)
}

fn sub3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn dot3(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn cross3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn normalize3(vector: [f32; 3]) -> [f32; 3] {
    let length = dot3(vector, vector).sqrt();
    [vector[0] / length, vector[1] / length, vector[2] / length]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct RecordingProcessor {
        listener: ListenerState,
        observed: Arc<Mutex<Vec<ListenerState>>>,
    }

    impl ListenerStateSink for RecordingProcessor {
        fn set_listener_state(&mut self, listener: ListenerState) {
            self.listener = listener;
        }
    }

    impl BlockProcessor for RecordingProcessor {
        fn block_size_frames(&self) -> usize {
            1
        }

        fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
            self.observed.lock().unwrap().push(self.listener);
            block.output_left[0] = 0.0;
            block.output_right[0] = 0.0;
            Ok(())
        }
    }

    #[test]
    fn listener_orientation_is_late_bound_for_each_audio_block() {
        let north = ListenerControl::at(
            EnuVector3::new(0.0, 0.0, 1.5),
            EnuVector3::new(0.0, 1.0, 0.0),
        )
        .listener_state(EnuVector3::default());
        let east = ListenerControl::at(
            EnuVector3::new(0.0, 0.0, 1.5),
            EnuVector3::new(1.0, 0.0, 0.0),
        )
        .listener_state(EnuVector3::default());
        let (mut mailbox, reader) = PoseMailbox::new(north);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let processor = RecordingProcessor {
            listener: north,
            observed: Arc::clone(&observed),
        };
        let (_gain_writer, gain_reader) = SnapshotPublication::new(0.0);
        let (meter_writer, _meter_reader) = SnapshotPublication::new(MeterReading::SILENT);
        let (audio_block_writer, _audio_block_reader) = SnapshotPublication::new(0_u64);
        let mut late = LateBoundProcessor::new(
            processor,
            reader,
            gain_reader,
            meter_writer,
            MeterAccumulator::new(48_000, 1, 0.5),
            audio_block_writer,
        );
        let mut left = [0.0];
        let mut right = [0.0];
        let mut render = |processor: &mut LateBoundProcessor<RecordingProcessor>| {
            processor
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &[],
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
        };
        render(&mut late);
        mailbox.publish(east);
        render(&mut late);
        assert_eq!(*observed.lock().unwrap(), vec![north, east]);
    }

    #[test]
    fn triangle_edges_are_deduplicated() {
        let mesh = AcousticMesh {
            vertices_enu_m: vec![
                EnuVector3::new(0.0, 0.0, 0.0),
                EnuVector3::new(1.0, 0.0, 0.0),
                EnuVector3::new(1.0, 1.0, 0.0),
                EnuVector3::new(0.0, 1.0, 0.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3]],
            material_ids: vec![0, 0],
        };
        assert_eq!(mesh_edges(&mesh).len(), 5);
    }

    #[test]
    fn listen_gain_converts_db_and_hard_clamps_output() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1.0e-6);
        assert!((db_to_linear(20.0) - 10.0).abs() < 1.0e-5);
        let mut left = [0.02, -0.02, 0.0];
        let mut right = [0.5, -0.5, 0.001];
        apply_output_gain(&mut left, &mut right, db_to_linear(40.0));
        assert_eq!(left, [1.0, -1.0, 0.0]);
        assert_eq!(right, [1.0, -1.0, 0.1]);
    }

    #[test]
    fn enable_mute_and_solo_gain_matrix_is_source_local_and_silence_wins() {
        let mut mix = SourceMix::ALL_AUDIBLE;
        assert_eq!(&mix.gains(3)[..3], &[1.0, 1.0, 1.0]);

        mix.enabled[0] = false;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 1.0, 1.0]);

        mix.muted[1] = true;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 0.0, 1.0]);

        mix.soloed[2] = true;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 0.0, 1.0]);

        mix.soloed[1] = true;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 0.0, 1.0]);

        mix.enabled[2] = false;
        assert_eq!(&mix.gains(3)[..3], &[0.0, 0.0, 0.0]);
    }

    #[test]
    fn disabled_solo_does_not_silence_enabled_sources() {
        let mut mix = SourceMix::ALL_AUDIBLE;
        mix.enabled[1] = false;
        mix.soloed[1] = true;
        assert_eq!(&mix.gains(3)[..3], &[1.0, 0.0, 1.0]);
    }

    #[test]
    fn source_trajectory_position_is_determined_by_elapsed_audio_blocks() {
        let trajectory = SourceTrajectory::from_fixture(&Trajectory {
            waypoints_m: vec![[0.0, 0.0, 1.5], [10.0, 0.0, 1.5], [10.0, 10.0, 1.5]],
            speed_mps: 2.0,
            max_speed_mps: Some(2.0),
        })
        .unwrap();

        let after_one_second = trajectory.sample_at_block(375);
        assert_eq!(after_one_second.position, EnuVector3::new(2.0, 0.0, 1.5));
        assert_eq!(after_one_second.direction, EnuVector3::new(1.0, 0.0, 0.0));
        let at_first_corner = trajectory.sample_at_block(1_875);
        assert_eq!(at_first_corner.position, EnuVector3::new(10.0, 0.0, 1.5));
        assert_eq!(at_first_corner.direction, EnuVector3::new(0.0, 1.0, 0.0));
        assert_eq!(
            trajectory.sample_at_block(375),
            trajectory.sample_at_block(375)
        );
    }

    #[test]
    fn meter_accumulates_peak_and_rms_over_rolling_window() {
        let mut meter = MeterAccumulator::new(4, 2, 1.0);
        let first = meter.observe(&[1.0, 0.0], &[0.0, 0.0]);
        assert_eq!(first.peak_dbfs, 0.0);
        assert!((first.rms_dbfs - -6.020_600_3).abs() < 1.0e-5);
        let second = meter.observe(&[0.5, 0.5], &[0.5, 0.5]);
        assert_eq!(second.peak_dbfs, 0.0);
        let third = meter.observe(&[0.0, 0.0], &[0.0, 0.0]);
        assert!((third.peak_dbfs - -6.020_600_3).abs() < 1.0e-5);
        assert!((third.rms_dbfs - -9.030_9).abs() < 1.0e-4);
    }

    #[test]
    fn autopilot_derives_inset_rectangle_and_moves_at_constant_speed() {
        let circuit = Bounds2 {
            min: [0.0, 0.0],
            max: [100.0, 60.0],
        }
        .inset_circuit();
        assert!((circuit.min[0] - 9.6).abs() < 1.0e-5);
        assert!((circuit.min[1] - 9.6).abs() < 1.0e-5);
        assert!((circuit.max[0] - 90.4).abs() < 1.0e-5);
        assert!((circuit.max[1] - 50.4).abs() < 1.0e-5);
        let start = circuit.sample(0.0);
        assert!((start.position[0] - 9.6).abs() < 1.0e-5);
        assert!((start.position[1] - 9.6).abs() < 1.0e-5);
        assert_eq!(start.direction, [1.0, 0.0]);
        let corner = circuit.sample(80.8).position;
        assert!((corner[0] - 90.4).abs() < 1.0e-5);
        assert!((corner[1] - 9.6).abs() < 1.0e-5);
        let northbound = circuit.sample(90.8).position;
        assert!((northbound[0] - 90.4).abs() < 1.0e-5);
        assert!((northbound[1] - 19.6).abs() < 1.0e-5);
        let a = circuit.sample(25.0).position;
        let b = circuit.sample(31.0).position;
        assert!(((b[0] - a[0]).hypot(b[1] - a[1]) - 6.0).abs() < 1.0e-6);
    }

    #[test]
    fn first_person_projection_places_known_points_and_clips_near_plane() {
        let projection = FirstPersonProjection::new(
            EnuVector3::new(0.0, 0.0, 1.5),
            0.0,
            FIRST_PERSON_VERTICAL_FOV_RADIANS,
            FIRST_PERSON_NEAR_M,
        );
        let rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(200.0, 100.0));
        let (center, distance) = projection
            .project_point(EnuVector3::new(0.0, 10.0, 1.5), rect)
            .unwrap();
        assert!((center.x - 100.0).abs() < 1.0e-6);
        assert!((center.y - 50.0).abs() < 1.0e-6);
        assert!((distance - 10.0).abs() < 1.0e-6);
        let right = projection
            .project_point(EnuVector3::new(1.0, 10.0, 1.5), rect)
            .unwrap()
            .0;
        assert!(right.x > center.x);
        assert!(
            projection
                .project_point(EnuVector3::new(0.0, -1.0, 1.5), rect)
                .is_none()
        );
        assert!(
            projection
                .project_segment(
                    EnuVector3::new(0.0, -1.0, 1.5),
                    EnuVector3::new(0.0, 1.0, 1.5),
                    rect,
                )
                .is_some()
        );
    }
}
