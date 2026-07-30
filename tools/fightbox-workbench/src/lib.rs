mod asset;
mod fixture;
mod pose;
mod workbench;

use std::path::PathBuf;
use std::time::Instant;

pub use pose::{ListenerControl, PoseMailbox};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchArgs {
    pub package: PathBuf,
    pub baked: PathBuf,
    pub fixture: PathBuf,
    pub device: Option<String>,
}

pub fn launch(args: LaunchArgs) -> Result<(), String> {
    let startup_started = Instant::now();
    let title = "Fightbox Workbench";
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size([1280.0, 820.0]),
        ..Default::default()
    };
    let app = workbench::Workbench::load(args, startup_started)?;
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
