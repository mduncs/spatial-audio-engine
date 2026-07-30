use std::{
    env, fs,
    path::{Path, PathBuf},
};

const VERSION_MAJOR: &str = "#define STEAMAUDIO_VERSION_MAJOR 4";
const VERSION_MINOR: &str = "#define STEAMAUDIO_VERSION_MINOR 8";
const VERSION_PATCH: &str = "#define STEAMAUDIO_VERSION_PATCH 1";

fn main() {
    println!("cargo:rustc-check-cfg=cfg(steam_audio_sdk_linked)");
    println!("cargo:rerun-if-env-changed=STEAM_AUDIO_SDK_DIR");

    if env::var_os("CARGO_FEATURE_LINKED_SDK").is_none() {
        return;
    }

    let sdk_root = env::var_os("STEAM_AUDIO_SDK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!(
                "feature `linked-sdk` requires STEAM_AUDIO_SDK_DIR. Run \
             scripts/acquire-steam-audio.sh, then export its printed path."
            )
        });
    let header = find_named_file(&sdk_root, "phonon.h", 4)
        .unwrap_or_else(|| panic!("Steam Audio SDK at {} has no phonon.h", sdk_root.display()));
    let version_header = header.parent().unwrap().join("phonon_version.h");
    validate_headers(&header, &version_header);

    let target_os =
        env::var("CARGO_CFG_TARGET_OS").expect("Cargo must provide CARGO_CFG_TARGET_OS");
    let target_arch =
        env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo must provide CARGO_CFG_TARGET_ARCH");
    let library_name = if target_os == "windows" {
        "phonon.lib"
    } else if target_os == "macos" {
        "libphonon.dylib"
    } else if target_os == "ios" {
        "libphonon.a"
    } else {
        "libphonon.so"
    };
    let library = find_target_library(&sdk_root, library_name, &target_os, &target_arch)
        .unwrap_or_else(|| {
            panic!(
                "Steam Audio SDK at {} has no {} library for target {}-{}",
                sdk_root.display(),
                library_name,
                target_arch,
                target_os,
            )
        });
    if target_os == "macos" {
        validate_macos_architecture(&library, &target_arch);
    }
    let library_dir = library.parent().expect("a library path has a parent");
    println!("cargo:rustc-link-search=native={}", library_dir.display());
    if target_os == "ios" {
        println!("cargo:rustc-link-lib=static=phonon");
    } else {
        println!("cargo:rustc-link-lib=dylib=phonon");
    }
    println!("cargo:library_dir={}", library_dir.display());
    // Development and test executables must find the exact verified SDK selected above.
    // Release packaging gets an explicit runtime-loader policy when a host exists.
    if target_os == "macos" || target_os == "linux" {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", library_dir.display());
    }
    println!("cargo:rustc-cfg=steam_audio_sdk_linked");
}

fn validate_headers(header: &Path, version_header: &Path) {
    let header_contents = fs::read_to_string(header)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", header.display()));
    let version_contents = fs::read_to_string(version_header).unwrap_or_else(|error| {
        panic!(
            "Steam Audio header {} has no readable phonon_version.h beside it: {error}",
            header.display()
        )
    });
    for marker in [VERSION_MAJOR, VERSION_MINOR, VERSION_PATCH] {
        assert!(
            version_contents.contains(marker),
            "Steam Audio version marker {marker:?} missing from {}",
            version_header.display()
        );
    }
    for marker in [
        "iplContextCreate",
        "iplContextRelease",
        "IPLContextSettings",
    ] {
        assert!(
            header_contents.contains(marker),
            "Steam Audio API marker {marker:?} missing from {}",
            header.display()
        );
    }
}

fn find_named_file(root: &Path, name: &str, remaining_depth: usize) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|file_name| file_name == name) {
            return Some(path);
        }
        if remaining_depth > 0 && path.is_dir() {
            if let Some(found) = find_named_file(&path, name, remaining_depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn find_target_library(
    root: &Path,
    name: &str,
    target_os: &str,
    target_arch: &str,
) -> Option<PathBuf> {
    let expected_directory = match (target_os, target_arch) {
        // Steam Audio's macOS binary is in lib/osx and may be universal.
        ("macos", _) => "osx",
        // Valve ships iPhoneOS arm64 objects only (no simulator slice).
        ("ios", "aarch64") => "ios",
        ("windows", "x86_64") => "windows-x64",
        ("windows", "x86") => "windows-x86",
        ("windows", "aarch64") => "windows-arm64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => return None,
    };
    let candidates = find_named_files(root, name, 5);
    let mut matching = candidates.into_iter().filter(|path| {
        path.components()
            .any(|part| part.as_os_str() == expected_directory)
    });
    let library = matching.next()?;
    assert!(
        matching.next().is_none(),
        "Steam Audio SDK contains multiple {expected_directory}/{name} candidates; refusing ambiguous link selection"
    );
    Some(library)
}

fn find_named_files(root: &Path, name: &str, remaining_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|file_name| file_name == name) {
            found.push(path);
        } else if remaining_depth > 0 && path.is_dir() {
            found.extend(find_named_files(&path, name, remaining_depth - 1));
        }
    }
    found
}

fn validate_macos_architecture(library: &Path, target_arch: &str) {
    let expected_lipo_arch = match target_arch {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("unsupported macOS target architecture {other}"),
    };
    let output = std::process::Command::new("lipo")
        .arg("-archs")
        .arg(library)
        .output()
        .unwrap_or_else(|error| {
            panic!("cannot run lipo to validate {}: {error}", library.display())
        });
    assert!(
        output.status.success(),
        "lipo could not inspect Steam Audio library {}",
        library.display()
    );
    let architectures = String::from_utf8_lossy(&output.stdout);
    assert!(
        architectures
            .split_whitespace()
            .any(|arch| arch == expected_lipo_arch),
        "Steam Audio library {} has architectures {:?}, not required target {expected_lipo_arch}",
        library.display(),
        architectures.trim()
    );
}
