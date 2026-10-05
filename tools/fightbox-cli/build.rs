use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"),
    );
    let workspace = manifest_dir.join("../..");
    let snapshot = canonical_source_snapshot(&workspace);
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR"))
        .join("fightbox-source-snapshot.bin");
    if fs::read(&out).ok().as_deref() != Some(snapshot.as_slice()) {
        fs::write(&out, snapshot).expect("write embedded source snapshot");
    }
    let base_commit = source_base_commit(&workspace);
    println!("cargo:rustc-env=FIGHTBOX_SOURCE_BASE_COMMIT={base_commit}");
    emit_git_rerun_paths(&workspace);

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
    let bundled_rpath = if target_os == "macos" {
        "@executable_path"
    } else {
        "$ORIGIN"
    };
    println!("cargo:rustc-link-arg=-Wl,-rpath,{bundled_rpath}");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{library_dir}");
}

fn canonical_source_snapshot(workspace: &Path) -> Vec<u8> {
    let mut files = Vec::new();
    for name in ["Cargo.toml", "Cargo.lock"] {
        let path = workspace.join(name);
        if path.is_file() {
            files.push(path);
        }
    }
    for name in ["crates", "tools"] {
        collect_source_files(&workspace.join(name), &mut files);
    }
    files.sort();
    let mut snapshot = b"fightbox.canonical-engine-source.v1\0".to_vec();
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = path
            .strip_prefix(workspace)
            .expect("source snapshot path must remain in workspace");
        let bytes = fs::read(&path).expect("read source snapshot input");
        let name = relative.to_string_lossy();
        snapshot.extend_from_slice(&(name.len() as u64).to_le_bytes());
        snapshot.extend_from_slice(name.as_bytes());
        snapshot.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        snapshot.extend_from_slice(&bytes);
    }
    snapshot
}

fn collect_source_files(root: &Path, files: &mut Vec<PathBuf>) {
    if !root.exists() {
        return;
    }
    let mut entries = fs::read_dir(root)
        .expect("read source snapshot directory")
        .collect::<std::io::Result<Vec<_>>>()
        .expect("read source snapshot entries");
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().expect("inspect source snapshot entry");
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_source_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            || path.file_name().is_some_and(|name| {
                name == "Cargo.toml" || name == "Cargo.lock" || name == "build.rs"
            })
        {
            files.push(path);
        }
    }
}

fn source_base_commit(workspace: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .current_dir(workspace)
        .output()
        .expect("resolve source base commit");
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(
        output.status.success()
            && identity.len() == 40
            && identity
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "acceptance-capable fightbox builds require a valid committed HEAD"
    );
    identity
}

fn emit_git_rerun_paths(workspace: &Path) {
    let git_dir = workspace.join(".git");
    let head = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head.display());
    if let Ok(contents) = fs::read_to_string(&head)
        && let Some(reference) = contents.strip_prefix("ref: ").map(str::trim)
    {
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join(reference).display()
        );
    }
}
