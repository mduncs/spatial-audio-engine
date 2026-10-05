//! Device-free render profiling of the authored combat cue graph.

use super::*;
use fightbox_runtime::backend::SimulationRunner;
use fightbox_runtime::live::LiveSpatialSourceBuffer;
use fightbox_runtime::ProgramProcessBlock;
use fightbox_steam_audio::{enable_render_profiling, render_profile_totals};
use serde::Serialize;

#[derive(Serialize)]
struct Distribution {
    median_us: f64,
    p99_us: f64,
    p999_us: f64,
    mean_us: f64,
}

fn distribution(values: impl Iterator<Item = u64>) -> Distribution {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable();
    let percentile = |p: f64| {
        values[((values.len() - 1) as f64 * p).round() as usize] as f64 / 1000.0
    };
    Distribution {
        median_us: percentile(0.5),
        p99_us: percentile(0.99),
        p999_us: percentile(0.999),
        mean_us: values.iter().map(|v| *v as f64).sum::<f64>() / values.len() as f64 / 1000.0,
    }
}

fn configured_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default))
}

/// One ignored evidence test, run serially in release mode. It never creates a
/// device, window, live input, or paced output worker.
#[test]
#[ignore = "retained combat package/media; set FIGHTBOX_COMBAT_PROFILE_OUT outside repository"]
fn combat_graph_offline_profile() {
    assert!(!cfg!(debug_assertions), "run this profile with --release");
    let output = std::env::var_os("FIGHTBOX_COMBAT_PROFILE_OUT")
        .map(PathBuf::from)
        .expect("FIGHTBOX_COMBAT_PROFILE_OUT must name an external evidence JSON file");
    validate_render_out(&output.with_extension("wav")).unwrap();
    let seconds = std::env::var("FIGHTBOX_COMBAT_PROFILE_SECONDS")
        .ok().map(|v| v.parse::<u64>().unwrap()).unwrap_or(90);
    let repeats = std::env::var("FIGHTBOX_COMBAT_PROFILE_REPEATS")
        .ok().map(|v| v.parse::<usize>().unwrap()).unwrap_or(3);
    assert!((1..=120).contains(&seconds) && (1..=9).contains(&repeats));
    let package_path = configured_path("FIGHTBOX_COMBAT_PROFILE_PACKAGE",
        "/path/to/spatial-audio/evidence/megablock-seed1/megablock.fightbox");
    let baked_path = configured_path("FIGHTBOX_COMBAT_PROFILE_BAKED",
        "/path/to/spatial-audio/evidence/astra-user-weak-street/road-sample-matrix/successor-path1500-vis40.baked");
    let fixture_path = configured_path("FIGHTBOX_COMBAT_PROFILE_FIXTURE",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/city/combat-reference/fixture.json"));
    let fixture = Fixture::read(&fixture_path).unwrap();
    assert!(!fixture.cues.is_empty());
    assert!(fixture.simulation.reflections.enabled);
    let package = read_package(&package_path).unwrap();
    let baked = load_baked(&baked_path, &package).unwrap();
    let mesh = scene_mesh(&package).unwrap();
    let assets = fixture.sources.iter().map(|source| {
        assert!(source.live_input.is_none(), "offline profile never opens inputs");
        (source.asset_id.clone(), load_asset(&source.asset_id).unwrap())
    }).collect::<BTreeMap<_, _>>();
    let names = ["input", "runtime_render", "backend_render", "preparation", "direct_hrtf",
        "pathing", "echoes", "reflections_decode"];
    let mut runs = Vec::new();
    enable_render_profiling(true);
    for repeat in 0..repeats {
        let listener = ListenerControl::at(fixture.initial_listener_position().unwrap(),
            to_enu(fixture.listener.forward_enu)).listener_state(EnuVector3::default());
        let mut profiles = Vec::new();
        let mut descriptors = Vec::new();
        let mut signals = Vec::new();
        let mut playback = Vec::new();
        let mut trajectories = Vec::new();
        let mut echo_sources = Vec::new();
        let mut motions = [SourceMotion::default(); fightbox_runtime::MAX_ACTIVE_SOURCES];
        let mut mix = SourceMix::ALL_AUDIBLE;
        mix.enabled.fill(false);
        mix.retrigger_generations.fill(1);
        for (index, source) in fixture.sources.iter().enumerate() {
            let asset = &assets[&source.asset_id];
            let pose = Pose { position: source.initial_position().unwrap(),
                forward: source.forward_enu_normalized().unwrap(), up: EnuVector3::new(0.0, 0.0, 1.0) };
            let profile = SourceProfile { id: SourceId::new(&source.id), pose,
                reference_level: source.reference_level.to_api(), asset_analysis: asset.analysis.clone(),
                extent: source.extent, directivity: source.directivity.to_api(),
                max_speed_mps: source.trajectory.as_ref().map_or(0.0,
                    |t| t.max_speed_mps.unwrap_or(t.speed_mps) as f32) };
            let impulse_class = if source.asset_id == "squad-a10-impacts" {
                fightbox_api::ImpulseClass::ArtilleryThunder
            } else { fightbox_api::ImpulseClass::None };
            let echo = asset.echo_profile(source.impulsive, impulse_class).unwrap();
            descriptors.push(MultiSourceDescriptor::at(pose.position)
                .with_reference_level(profile.reference_level).with_directivity(profile.directivity)
                .with_extent(profile.extent).with_echo_profile(echo));
            echo_sources.push(echo.is_enabled().then_some(pose.position));
            motions[index] = SourceMotion { active: true, pose, linear_velocity_mps: EnuVector3::default() };
            mix.monitor_gains[index] = monitor_offset_gain(source.monitor_offset_db);
            playback.push(SourcePlayback::for_asset(&source.asset_id, SAMPLE_RATE,
                source.playback_start_offset_s, asset.samples.len(), source.restart_on_enable, asset.loops));
            trajectories.push(source.trajectory.as_ref().map(SourceTrajectory::from_fixture).transpose().unwrap());
            profiles.push(profile);
            signals.push(asset.samples.clone());
        }
        let mut cracks = Vec::new();
        let mut ballistic = Vec::new();
        for (parent_index, source) in fixture.sources.iter().enumerate() {
            if let Some(flight) = &source.ballistic {
                let slot = profiles.len();
                let declaration = BallisticCrack::declare(parent_index, slot, source, flight,
                    &signals[parent_index], SAMPLE_RATE, BLOCK_SIZE, listener.pose.position).unwrap();
                descriptors.push(declaration.descriptor);
                profiles.push(declaration.profile);
                motions[slot] = SourceMotion { active: false, pose: declaration.pose,
                    linear_velocity_mps: EnuVector3::default() };
                cracks.push(declaration.playback);
                ballistic.push(declaration.crack);
            }
        }
        let (mut safety, safety_reader) = configure_output_safety(listener.pose.position, &profiles).unwrap();
        safety.set_monitor_gain_db(30.0).unwrap();
        for crack in &mut ballistic {
            let armed = crack.arm(1, listener.pose.position).unwrap();
            if let Some(armed_crack) = armed.crack {
                safety.set_source(crack.slot_index, &armed_crack.profile, None).unwrap();
                motions[crack.slot_index].active = true;
                motions[crack.slot_index].pose.position = armed_crack.profile.pose.position;
                mix.retrigger_delay_frames[crack.parent_index] = armed.impact_delay_frames;
            }
        }
        let mut simulation_config = fixture.simulation_config();
        tune_reflection_workers(&mut simulation_config, fixture.sources.len());
        let (mut runner, mut backend) = build_multi_source_session(&mesh, &baked,
            AudioConfig { sample_rate_hz: SAMPLE_RATE as i32, frame_size: BLOCK_SIZE as i32 },
            simulation_config, &descriptors).unwrap();
        let scene_reset = backend.scene_reset_control();
        let (mut host_echoes, shot_trigger) = crate::echo_paths::HostEchoes::start(
            backend.take_echo_trigger_control().unwrap(), &package.mesh, &package.materials,
            &echo_sources, listener.pose.position, fixture.air_exponents()).unwrap();
        let initial = PropagationSnapshot { sequence: 1, simulated_at_ns: u64::MAX,
            sources: std::array::from_fn(|index| SourcePropagation { active: index < profiles.len(),
                target_delay_samples: 0.0, left_gain: 1.0, right_gain: 1.0 }) };
        let (_propagation_writer, propagation_reader) = SnapshotPublication::new(initial);
        let config = EngineConfig { sample_rate_hz: SAMPLE_RATE, block_size_frames: BLOCK_SIZE,
            max_active_sources: profiles.len() as u8, ..EngineConfig::default() };
        let mut graph = RuntimeGraph::new_with_backend_and_output_safety(config,
            propagation_reader, safety_reader, Box::new(backend)).unwrap();
        graph.set_listener_state(listener);
        for (index, profile) in profiles.iter().enumerate() {
            graph.set_source(index, profile, SceneCalibration::default()).unwrap();
        }
        if fixture.sources.len() > 1 {
            runner.prepare_simulation_for_realtime(&SimulationUpdate { listener, sources: motions }).unwrap();
            let silent = [0.0; BLOCK_SIZE as usize];
            let sources = (0..profiles.len()).map(|source_index| {
                fightbox_runtime::backend::SpatialProgramBlock {
                    source_index, program_plane_count: 1, program_planes: [&silent, &[]],
                }
            }).collect::<Vec<_>>();
            let mut left = silent;
            let mut right = silent;
            // Match control-side startup warming; exclude it from measured blocks.
            for _ in 0..16 {
                graph.process_program_block(ProgramProcessBlock {
                    now_ns: 0, sources: &sources,
                    output_left: &mut left, output_right: &mut right,
                }).unwrap();
            }
        }
        let (_mix_writer, mix_reader) = SnapshotPublication::new(mix);
        let control = SceneControl { generation: 1, running: true, listener: listener.pose.position,
            prepared_generations: mix.retrigger_generations, delay_frames: mix.retrigger_delay_frames };
        let (_scene_writer, scene_reader) = SnapshotPublication::new(control);
        let (status_writer, mut status_reader) = SnapshotPublication::new(PlaybackSnapshot::default());
        let (trace_writer, _trace_reader) = SnapshotPublication::new(PlaybackSnapshot::default());
        let mut input = WorkbenchInput { audio_sample: 0, signals,
            program_plane_counts: vec![1; fixture.sources.len()], live_inputs: (0..fixture.sources.len()).map(|_| None).collect(),
            song_readers: (0..fixture.sources.len()).map(|_| crate::song_program::song_channel().1).collect(),
            live_mono: vec![false; fixture.sources.len()],
            playback, cracks, scene: Some(SceneTimeline::new(&fixture, SAMPLE_RATE)),
            scene_control_reader: scene_reader, scene_reset, source_mix_reader: mix_reader,
            playback_status_writer: status_writer, trace_playback_writer: trace_writer,
            echo_trigger: Some(shot_trigger) };
        let mut staging = LiveSpatialSourceBuffer::new(BLOCK_SIZE as usize);
        let mut left = [0.0; BLOCK_SIZE as usize];
        let mut right = [0.0; BLOCK_SIZE as usize];
        let blocks = seconds * u64::from(SAMPLE_RATE) / u64::from(BLOCK_SIZE);
        let mut rows = Vec::<[u64; 12]>::with_capacity(blocks as usize);
        let mut quality = Vec::new();
        let mut last_quality = String::new();
        let mut pass_clock = [u64::MAX; 3];
        let mut last_reflection_sample = 0_u64;
        let mut reflection_positions = motions.map(|m| m.pose.position);
        let mut simulation_ns = [0_u64; 3];
        let mut simulation_passes = [0_u64; 3];
        let mut pcm_hash = 0xcbf29ce484222325_u64;
        for block in 0..blocks {
            let audio_sample = block * u64::from(BLOCK_SIZE);
            let consumed = status_reader.read();
            for (index, trajectory) in trajectories.iter().enumerate() {
                if let Some(trajectory) = trajectory {
                    let status = consumed.sources[index];
                    let scene_frames = if status.enabled {
                        status.audio_sample.saturating_sub(status.trigger_audio_sample)
                    } else { 0 };
                    let sample = trajectory.sample_at_frame(scene_frames);
                    motions[index].pose.position = sample.position;
                    motions[index].pose.forward = sample.direction;
                    motions[index].linear_velocity_mps = if status.enabled {
                        scale(sample.direction, trajectory.speed_mps)
                    } else { EnuVector3::default() };
                    safety.set_source_position(index, sample.position).unwrap();
                    if echo_sources[index].is_some() {
                        host_echoes.move_source(index, sample.position, listener.pose.position);
                    }
                }
            }
            runner.update_inputs(&SimulationUpdate { listener, sources: motions });
            let cadences = SimulationCadences::default();
            let moved = motions.iter().zip(reflection_positions).any(|(motion, previous)|
                motion.active && vector_length(subtract(motion.pose.position, previous)) >= cadences.reflection_max_displacement_m);
            let periods = [cadences.direct_hz, cadences.pathing_hz, cadences.reflections_hz];
            for pass in 0..3 {
                let clock = audio_sample * u64::from(periods[pass]) / u64::from(SAMPLE_RATE);
                let motion_due = pass == 2 && moved && audio_sample.saturating_sub(last_reflection_sample)
                    * u64::from(cadences.reflection_max_hz) >= u64::from(SAMPLE_RATE);
                if clock != pass_clock[pass] || motion_due {
                    let started = Instant::now();
                    match pass { 0 => runner.run_direct(), 1 => runner.run_pathing(), _ => runner.run_reflections() }.unwrap();
                    simulation_ns[pass] += started.elapsed().as_nanos() as u64;
                    simulation_passes[pass] += 1;
                    pass_clock[pass] = clock;
                    if pass == 2 {
                        last_reflection_sample = audio_sample;
                        reflection_positions = motions.map(|m| m.pose.position);
                    }
                }
            }
            let governor = runner.quality_governor_telemetry().unwrap();
            let key = format!("{} {:?} {} rays {} bounces cadence {} IR {:.3} gain {:.3}",
                governor.ladder_position, governor.reflections.level, governor.reflections.rays,
                governor.reflections.bounces, governor.reflections.cadence_divisor,
                governor.reflections.ir_duration_s, governor.reflection_output_gain);
            if key != last_quality {
                quality.push(serde_json::json!({ "block": block, "seconds": audio_sample as f64 / f64::from(SAMPLE_RATE), "quality": key }));
                last_quality = key;
            }
            staging.clear();
            let input_started = Instant::now();
            input.fill_sources(&mut staging);
            let input_ns = input_started.elapsed().as_nanos() as u64;
            let sources = staging.source_blocks();
            let nonzero_programs = sources[..staging.len()].iter()
                .filter(|source| source.program_planes[0].iter().any(|sample| *sample != 0.0)).count() as u64;
            let before = render_profile_totals();
            let started = Instant::now();
            graph.process_program_block(ProgramProcessBlock {
                now_ns: audio_sample * 1_000_000_000 / u64::from(SAMPLE_RATE),
                sources: &sources[..staging.len()], output_left: &mut left, output_right: &mut right,
            }).unwrap();
            let graph_ns = started.elapsed().as_nanos() as u64;
            let after = render_profile_totals();
            rows.push([block, input_ns, graph_ns, after.total_ns - before.total_ns,
                after.preparation_ns - before.preparation_ns, after.direct_ns - before.direct_ns,
                after.path_ns - before.path_ns, after.echo_ns - before.echo_ns,
                after.reflection_ns - before.reflection_ns, after.source_count - before.source_count,
                after.echo_tap_count - before.echo_tap_count, nonzero_programs]);
            for sample in left.iter().chain(&right) {
                assert!(sample.is_finite());
                for byte in sample.to_bits().to_le_bytes() {
                    pcm_hash = (pcm_hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
            }
        }
        let distributions = names.iter().enumerate().map(|(index, name)|
            (*name, distribution(rows.iter().map(|row| row[index + 1])))).collect::<BTreeMap<_, _>>();
        eprintln!("combat profile repeat {}: render median {:.1} us p99 {:.1} us p99.9 {:.1} us",
            repeat + 1, distributions["runtime_render"].median_us,
            distributions["runtime_render"].p99_us, distributions["runtime_render"].p999_us);
        runs.push(serde_json::json!({ "repeat": repeat + 1, "distributions": distributions,
            "simulation_ns": simulation_ns, "simulation_passes": simulation_passes,
            "quality_timeline": quality, "pcm_fnv1a64": format!("{pcm_hash:016x}"),
            "rows": rows }));
    }
    enable_render_profiling(false);
    let report = serde_json::json!({ "schema_version": 1,
        "method": "unpaced audio clock; actual combat assets/cues, ballistic crack, host echoes, source motion, runtime safety/limiter; synchronous simulation; render timing not fed to governor",
        "build": "release", "seconds": seconds, "repeats": repeats,
        "sample_rate_hz": SAMPLE_RATE, "block_size_frames": BLOCK_SIZE,
        "package": package_path, "baked": baked_path, "fixture": fixture_path,
        "row_columns": ["block", "input_ns", "runtime_render_ns", "backend_render_ns", "preparation_ns",
            "direct_hrtf_ns", "pathing_ns", "echoes_ns", "reflections_decode_ns", "source_count", "echo_tap_count", "nonzero_input_programs"],
        "runs": runs });
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    eprintln!("combat profile saved {}", output.display());
}
