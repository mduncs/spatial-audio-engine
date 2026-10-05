use std::{env, fs, path::PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo provides CARGO_MANIFEST_DIR"),
    );
    let config_path = manifest_dir.join("cbindgen.toml");
    let canonical_header = manifest_dir.join("include/fightbox.h");
    let generated_header =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR")).join("fightbox.h");
    let config = cbindgen::Config::from_file(&config_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", config_path.display()));
    cbindgen::generate_with_config(&manifest_dir, config)
        .unwrap_or_else(|error| panic!("failed to generate Fightbox C header: {error}"))
        .write_to_file(&generated_header);
    let generated = fs::read(&generated_header)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", generated_header.display()));
    let canonical = fs::read(&canonical_header)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", canonical_header.display()));
    if canonical != generated {
        panic!(
            "canonical C header is stale; replace {} with the generated file at {}",
            canonical_header.display(),
            generated_header.display(),
        );
    }
    println!("cargo:rerun-if-changed={}", config_path.display());
    println!("cargo:rerun-if-changed={}", canonical_header.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src/lib.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src/v2_abi.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src/v3_abi.rs").display()
    );

    // The backend's native-library directory is links metadata. Its rpath
    // directive does not propagate to this crate's host test executables.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && let Some(directory) = env::var_os("DEP_FIGHTBOX_STEAM_AUDIO_LIBRARY_DIR")
    {
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}",
            PathBuf::from(directory).display()
        );
    }
}
