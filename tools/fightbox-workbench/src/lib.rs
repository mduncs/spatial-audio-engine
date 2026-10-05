// The headless replay report is one large serde_json literal.
#![recursion_limit = "256"]

pub mod acoustic_feed;
mod acoustic_state;
mod acoustic_view;
mod anomaly_field;
mod ambix;
#[cfg(all(test, feature = "linked-sdk"))]
mod ambix_evidence;
mod spatial_export;
mod asset;
mod ballistic_crack;
mod capture;
mod echo_paths;
mod fixture;
mod ground_map;
mod head_tracking;
mod level_trace;
mod level_trace_ui;
#[cfg(feature = "live-output")]
mod live_input;
#[cfg(feature = "live-output")]
mod app_tap;
mod mix_defaults;
mod pose;
mod quiet_output;
mod scene;
mod scene_positions;
mod song_program;
mod map_link;
mod source_drag;
mod walk_view;
mod workbench;

use std::path::PathBuf;
use std::time::Instant;

pub use pose::{ListenerControl, PoseMailbox};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayOptions {
    pub seconds: u32,
    pub monitor_gain_db: i32,
    /// Diagnostic only: hold Full quality while retaining real timing telemetry.
    pub pin_full_quality: bool,
    pub capture_root: PathBuf,
    /// Stand still at a street spot and press Play once instead of walking.
    pub shot_spot: Option<ReplayShotSpot>,
}

/// Street comparison spots, matching the Spot A/B buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayShotSpot {
    A,
    B,
}

impl ReplayShotSpot {
    pub fn east_m(self) -> f32 {
        match self {
            Self::A => 434.02,
            Self::B => 438.02,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuietAuditionOptions {
    pub seconds: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenderFormat {
    #[default]
    Binaural,
    Ambix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchArgs {
    pub package: PathBuf,
    pub baked: PathBuf,
    pub fixtures: Vec<PathBuf>,
    pub start_audio: bool,
    pub device: Option<String>,
    pub null_output: bool,
    pub render_out: Option<PathBuf>,
    pub render_format: RenderFormat,
    /// Test-only real-time WAV feeder; never opens an input device.
    pub live_input_wav: Option<PathBuf>,
    /// Test-only song drop before replay Play; never opens an input device.
    pub program_file: Option<PathBuf>,
    pub replay: Option<ReplayOptions>,
    pub quiet_audition: Option<QuietAuditionOptions>,
}

pub fn launch(args: LaunchArgs) -> Result<(), String> {
    let startup_started = Instant::now();
    if args.render_format == RenderFormat::Ambix
        && (args.render_out.is_none() || args.replay.is_none() || !args.null_output)
    {
        return Err("AmbiX requires headless --render-out with --null-output".into());
    }
    if args.replay.is_some() {
        return workbench::WorkbenchApp::run_headless(args, startup_started);
    }
    let title = "Fightbox Workbench";
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1280.0, 820.0]),
        ..Default::default()
    };
    let app = workbench::WorkbenchApp::load(args, startup_started)?;
    let window_started = Instant::now();
    eframe::run_native(
        title,
        options,
        Box::new(move |_| {
            eprintln!(
                "[startup] window + wgpu bring-up: {} ms",
                window_started.elapsed().as_millis()
            );
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| format!("cannot open workbench window: {error}"))
}
