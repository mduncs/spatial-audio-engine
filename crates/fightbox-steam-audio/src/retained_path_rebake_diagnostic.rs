//! Explicitly invoked retained-layout successor bake. No audio or default test work.
//! Root must time-box the owned test process; SDK bake cancellation is not wired.
use crate::{
    AcousticMaterial, BakedProbeBatch, EnuVector3, ExplicitProbe, ExplicitProbeBakeRequest,
    PROBE_BATCH_METADATA_SCHEMA, PathBakeConfig, ProbeBatchMetadata, STEAM_AUDIO_UPSTREAM_COMMIT,
    STEAM_AUDIO_VERSION, SceneMesh, bake_explicit_probe_batch,
};
use fightbox_evidence::sha256_hex;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Instant,
};

const OLD_SHA: &str = "fdc01dd62720131aa051f0554040f27d410cfe8594fa3b5f4bc45fbe3345786e";
const MESH_SHA: &str = "f18eeaf9a0dc54d9786dd5bd742138ad06053de89ec2f35effa2a50d74837a59";
const MATERIALS_SHA: &str = "eb92a1e6b465963e2897b1f75a2bf3bd7f538c513881374bd4819a31be730204";
const PROBE_COUNT: usize = 9723;
fn required_path(name: &str) -> PathBuf {
    let p = PathBuf::from(env::var_os(name).unwrap_or_else(|| panic!("set {name} explicitly")));
    assert!(p.is_absolute(), "{name} must be absolute");
    p
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn load_mesh(package: &Path) -> SceneMesh {
    let b = fs::read(package.join("mesh.bin")).unwrap();
    let material_bytes = fs::read(package.join("materials.json")).unwrap();
    assert_eq!(sha256_hex(&b), MESH_SHA);
    assert_eq!(sha256_hex(&material_bytes), MATERIALS_SHA);
    assert_eq!(&b[..8], b"FBXMESH\0");
    assert_eq!(u32_at(&b, 8), 1);
    let nv = u32_at(&b, 12) as usize;
    let nt = u32_at(&b, 16) as usize;
    assert_eq!(b.len(), 20 + nv * 12 + nt * 16);
    let vertices = (0..nv)
        .map(|i| {
            let o = 20 + i * 12;
            EnuVector3::new(
                f32::from_bits(u32_at(&b, o)),
                f32::from_bits(u32_at(&b, o + 4)),
                f32::from_bits(u32_at(&b, o + 8)),
            )
        })
        .collect();
    let triangles = (0..nt)
        .map(|i| {
            std::array::from_fn(|k| {
                i32::try_from(u32_at(&b, 20 + nv * 12 + i * 12 + k * 4)).unwrap()
            })
        })
        .collect();
    let material_indices = (0..nt)
        .map(|i| i32::try_from(u32_at(&b, 20 + nv * 12 + nt * 12 + i * 4)).unwrap())
        .collect();
    let v: Value = serde_json::from_slice(&material_bytes).unwrap();
    // World v1 material indices are lexicographic material-name order, independent
    // of serde_json map feature flags. Decode actual values, never replacement presets.
    let mut entries: Vec<_> = v.as_object().unwrap().iter().collect();
    entries.sort_by_key(|(name, _)| *name);
    let band = |v: &Value| std::array::from_fn(|i| v[i].as_f64().unwrap() as f32);
    let materials = entries
        .into_iter()
        .map(|(_, v)| AcousticMaterial {
            absorption: band(&v["absorption"]),
            scattering: v["scattering"].as_f64().unwrap() as f32,
            transmission: band(&v["transmission"]),
        })
        .collect();
    SceneMesh {
        vertices_enu_m: vertices,
        triangles,
        material_indices,
        materials,
    }
}
fn load_batch(path: &Path) -> BakedProbeBatch {
    let v: Value =
        serde_json::from_slice(&fs::read(path.join("probe-batch-metadata.json")).unwrap()).unwrap();
    assert_eq!(v["schema_version"], PROBE_BATCH_METADATA_SCHEMA);
    assert_eq!(v["steam_audio_version"], STEAM_AUDIO_VERSION);
    assert_eq!(v["upstream_commit"], STEAM_AUDIO_UPSTREAM_COMMIT);
    let n = |key: &str| v[key].as_u64().unwrap();
    let b = BakedProbeBatch {
        bytes: fs::read(path.join("probe-batch.bin")).unwrap(),
        metadata: ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: u32::try_from(n("probe_count")).unwrap(),
            path_data_size_bytes: n("path_data_size_bytes"),
            serialized_size_bytes: n("serialized_size_bytes"),
            content_sha256: v["content_sha256"].as_str().unwrap().into(),
            bake_progress_callback_count: u32::try_from(n("bake_progress_callback_count")).unwrap(),
            final_bake_progress_millionths: u32::try_from(n("final_bake_progress_millionths"))
                .unwrap(),
        },
    };
    b.validate().unwrap();
    assert_eq!(b.metadata.content_sha256, OLD_SHA);
    b
}
fn sphere_bits(probes: &[ExplicitProbe]) -> Vec<u8> {
    probes
        .iter()
        .flat_map(|p| {
            [
                p.center_enu_m.x,
                p.center_enu_m.y,
                p.center_enu_m.z,
                p.radius_m,
            ]
        })
        .flat_map(|v| v.to_bits().to_le_bytes())
        .collect()
}
fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[test]
#[ignore = "explicit retained9723-probe path rebake; root must authorize and time-box"]
fn retained_dual_path1500_successor() {
    let package = required_path("FIGHTBOX_REBAKE_PACKAGE");
    let input = required_path("FIGHTBOX_REBAKE_INPUT");
    let output = required_path("FIGHTBOX_REBAKE_OUTPUT");
    assert!(!output.exists(), "refuse to replace any existing output");
    let parent = output.parent().unwrap().canonicalize().unwrap();
    // Reject repo-local artifacts even if an output-parent symlink resolves there.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .canonicalize()
        .unwrap();
    assert!(
        !parent.starts_with(repo),
        "bake artifacts must be outside repository"
    );
    assert_ne!(
        parent.join(output.file_name().unwrap()),
        input.canonicalize().unwrap()
    );
    let staging = parent.join(format!(
        "{}.incomplete-{}",
        output.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    fs::create_dir(&staging).unwrap(); // Visible recoverability checkpoint on failure/termination.
    let mesh = load_mesh(&package);
    let old = load_batch(&input);
    let probes: Vec<_> = old
        .probe_coverage()
        .unwrap()
        .spheres()
        .map(|(c, r)| ExplicitProbe::new(c, r))
        .collect();
    assert_eq!(probes.len(), PROBE_COUNT);
    let layout_sha = sha256_hex(&sphere_bits(&probes));
    let config = PathBakeConfig {
        num_visibility_samples: 1,
        probe_visibility_radius_m: 0.0,
        visibility_threshold: 0.5,
        visibility_range_m: 40.0,
        path_range_m: 1500.0,
        num_threads: 4,
    };
    let request_json = json!({"schema_version":"fightbox.retained-path-rebake-request.v1",
        "input":input,"package":package,"output":output,"old_batch_sha256":OLD_SHA,
        "old_creation_config":null,"old_creation_config_note":"unknown; do not infer from different tall bake",
        "mesh_sha256":MESH_SHA,"materials_sha256":MATERIALS_SHA,"ordered_enu_sphere_bits_sha256":layout_sha,
        "probe_count":PROBE_COUNT,"pathing":{"path_range_m":1500,"visibility_range_m":40,
        "num_visibility_samples":1,"probe_visibility_radius_m":0,"visibility_threshold":0.5,"num_threads":4},
        "non_claims":["1500m is chosen headroom, not proof of every city geodesic","same spheres preserve geometric membership, not acoustic paths","root enforces600s process timeout; no SDK cancellation hook"]});
    write_json(&staging.join("rebake-request.json"), &request_json);
    // Release the old465MB byte vector before SDK compute. New request keeps only spheres.
    drop(old);
    eprintln!(
        "RETAINED_REBAKE_START staging={} probes={} range1500 vis40 threads4",
        staging.display(),
        PROBE_COUNT
    );
    let started = Instant::now();
    let baked = bake_explicit_probe_batch(&ExplicitProbeBakeRequest {
        mesh,
        probes: probes.clone(),
        pathing: config,
    })
    .unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    baked.validate().unwrap();
    let actual: Vec<_> = baked
        .probe_coverage()
        .unwrap()
        .spheres()
        .map(|(c, r)| ExplicitProbe::new(c, r))
        .collect();
    assert_eq!(
        sphere_bits(&actual),
        sphere_bits(&probes),
        "all ordered f32 sphere bits must remain exact"
    );
    assert!(
        baked.bytes.len() <= 1024 * 1024 * 1024,
        "diagnostic artifact exceeded1GiB budget; keep staging evidence"
    );
    fs::write(staging.join("probe-batch.bin"), &baked.bytes).unwrap();
    fs::write(
        staging.join("probe-batch-metadata.json"),
        baked.metadata.to_json(),
    )
    .unwrap();
    write_json(
        &staging.join("city-bake-manifest.json"),
        &json!({"schema_version":"fightbox.city-bake.v1",
        "materials_content_sha256":MATERIALS_SHA,"mesh_content_sha256":MESH_SHA,
        "probe_batch_sha256":baked.metadata.content_sha256}),
    );
    write_json(
        &staging.join("rebake-result.json"),
        &json!({"elapsed_seconds":elapsed,
        "probe_count":actual.len(),"ordered_sphere_bits_identical":true,"ordered_enu_sphere_bits_sha256":layout_sha,
        "new_batch_sha256":baked.metadata.content_sha256,"serialized_bytes":baked.bytes.len(),
        "path_bytes":baked.metadata.path_data_size_bytes,"request":request_json}),
    );
    // Destination was absent on entry; create no replacements. Caller owns this unique path.
    assert!(!output.exists());
    fs::rename(&staging, &output).unwrap();
    eprintln!(
        "RETAINED_REBAKE_DONE output={} seconds={elapsed:.3} bytes={} sha={}",
        output.display(),
        baked.bytes.len(),
        baked.metadata.content_sha256
    );
}
