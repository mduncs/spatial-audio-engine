use std::env;
use std::path::PathBuf;

fn main() {
    configure_governor_telemetry();
    println!("cargo:rerun-if-env-changed=DEP_FIGHTBOX_STEAM_AUDIO_LIBRARY_DIR");

    if env::var_os("CARGO_FEATURE_LINKED_SDK").is_none() {
        return;
    }

    let target_os =
        env::var("CARGO_CFG_TARGET_OS").expect("Cargo must provide CARGO_CFG_TARGET_OS");
    if target_os != "macos" && target_os != "linux" {
        return;
    }

    let library_dir = env::var("DEP_FIGHTBOX_STEAM_AUDIO_LIBRARY_DIR").expect(
        "linked-sdk requires the audited Steam Audio backend to publish its library directory",
    );
    println!("cargo:rustc-link-arg=-Wl,-rpath,{library_dir}");
}

/// The governor telemetry lane can land independently of the workbench lane.
/// Probe the path dependency's public shape so either revision remains a valid
/// build, without adding a feature that falsely claims backend behavior.
fn configure_governor_telemetry() {
    println!("cargo:rustc-check-cfg=cfg(fightbox_governor_boot_telemetry)");
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"),
    );
    let governor = manifest_dir.join("../../crates/fightbox-steam-audio/src/governor.rs");
    println!("cargo:rerun-if-changed={}", governor.display());
    let Ok(source) = std::fs::read_to_string(governor) else {
        return;
    };
    let boot_fields = [
        "pub boot_reflection_level:",
        "pub boot_predicted_cost_ns:",
        "pub boot_p99_budget_ns:",
        "pub boot_cost_limit_ns:",
    ];
    if boot_fields.iter().all(|field| source.contains(field)) {
        println!("cargo:rustc-cfg=fightbox_governor_boot_telemetry");
    }
}
