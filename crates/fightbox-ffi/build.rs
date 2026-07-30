use std::{env, path::PathBuf};

fn main() {
    // The backend's native-library directory is links metadata. Its rpath
    // directive does not propagate to this crate's host test executables.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        if let Some(directory) = env::var_os("DEP_FIGHTBOX_STEAM_AUDIO_LIBRARY_DIR") {
            println!(
                "cargo:rustc-link-arg=-Wl,-rpath,{}",
                PathBuf::from(directory).display()
            );
        }
    }
}
