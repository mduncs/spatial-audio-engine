use std::env;

fn main() {
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
