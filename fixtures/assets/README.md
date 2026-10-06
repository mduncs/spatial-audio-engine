# Deterministic asset descriptors

This directory holds the **text descriptors** for the deterministic Phase A signals that
`fightbox-evidence` regenerates into finite, calibrated PCM. A descriptor commits no binary
media: the same descriptor plus `fightbox_evidence::signal` always reproduce the same samples,
so a stem hash is stable.

## Files

- `asset.schema.json` — the strict Draft 2020-12 schema. It pins the module, restricts
  `channels`/`sample_rate_hz`/`duration_s`, forces `target_rms_dbfs` below 0 dBFS, and — via
  `if`/`then` rules — requires the selected `kind` to carry its matching generator block.
- `source-asset.schema.json` — the strict `fightbox.source-asset.v1` CLI authoring/import
  contract. It keeps asset layout, scene geometry compatibility, and
  presentation provenance independent; binds original and canonical identities and the pinned
  decode/resample toolchain; records channel/aggregate levels, stereo PCA/correlation/mono-fold
  admission, motion, rights, and seekability; and fixes canonical PCM at 48 kHz planar f32.
- `source-contract-authored-stereo.json` — a metadata-only authored-stereo contract fixture. Its
  hashes are deterministic schema examples, not claims that corresponding media is checked in.
- `validate.py` — a dependency-free validator. It enforces the schema's structural rules plus
  the cross-field rules JSON Schema cannot express: the generator carries *exactly* its kind's
  block (no mismatched kind/generator), frequencies are finite, positive, unique, and below
  Nyquist for the declared sample rate, optional onset timestamps are finite, strictly ascending,
  and inside the asset duration, and the mandatory no-delivered-ear-SPL non-claim is present.
- `s0-approach-sine.json`, `s0-calibrated-pink.json`, `s3-calibrated-pink.json`,
  `s3-multitone.json` — the current descriptor files. The last file's asset ID is
  `s3-multitone-spectral`.

Run from the repository root:

```sh
jq empty fixtures/assets/asset.schema.json fixtures/assets/*.json
python3 fixtures/assets/validate.py
git diff --check
```

`fightbox.asset-descriptor.v1` remains the byte-compatible Phase A input. The Rust parser
deterministically regenerates and normalizes a legacy mono descriptor into the same serialized
`SourceAssetDescriptor` truth used by `fightbox.source-asset.v1`. This remains a private CLI type:
the future importer validates it and emits planar channel blocks across the public runtime seam.
Direct stereo contracts validate today, but the current loader reports an explicit mono-only
capability error because Phase A remains frozen. Canonical mono and stereo PCM WAV assets use the
separate worker-side packer:

```sh
fightbox asset pack \
  --descriptor /absolute/path/source-asset.json \
  --media /absolute/path/source.wav \
  --output /absolute/path/canonical-package
fightbox asset inspect /absolute/path/canonical-package
```

The first production tranche accepts one- or two-channel PCM WAV at 48 kHz. It verifies the
original content hash and the complete channel-major planar PCM identity, writes atomic indexed
one-second chunks, chooses zstd level 3 only when it is smaller than raw `f32le`, and records both
stored and uncompressed hashes. `asset inspect` decompresses every chunk and verifies the complete
aggregate identity. The worker-side reader has a bounded one-second-chunk cache suitable for the
planned 2–4 second per-source residency. AIFF, CAF, FLAC, AAC/M4A, MP3, and non-48-kHz inputs remain
scheduled decoder/resampler adapters; they are rejected explicitly instead of being converted by
an opportunistic platform codec.

## Workbench personal songs on macOS

Drop a file onto a map source, otherwise the first live/music source (or first source), then press Play. A scene source can instead declare `"program_file": "/absolute/personal/song.m4a"` with `"default_enabled": false`; relative source paths resolve beside the fixture. For a descriptor under ignored `fixtures/assets/music/`, use an asset ID such as `music/my-song`, `"kind": "song"`, `"generator": {"song": {"path": "/absolute/personal/song.m4a"}}`, decoded `sample_rate_hz: 48000`, channels 1 or 2, and `target_rms_dbfs: -14` with the existing descriptor metadata. Descriptor-relative paths resolve from the repository root.

AudioToolbox decodes MP3, M4A/AAC, ALAC, FLAC, AIFF, WAV and CAF off the UI/audio threads. Workbench measures and normalizes the program to nominal −14 dBFS before the existing calibrated source drive; songs loop, retain stereo with `stereo_image`, and otherwise fold to measured mono. Ogg/Vorbis and non-macOS song decoding return clear errors. Personal files stay external; the pinned WAV route is unchanged. Whole-file PCM is held in memory (about 33 MiB/minute for stereo plus temporary decode/cache copies).

## Level terminology (read carefully)

Three distinct level concepts are in play. Confusing them is the classic evidence error, so the
descriptor and the generator keep them separate:

- **`target_rms_dbfs`** — the **delivered** RMS the regenerated asset measures after calibration
  (e.g. `-20`). This is the level the deterministic asset is *built to*.
- **`expected_reference_rms_dbfs`** — the generator's **raw pre-gain** RMS in dBFS, measured
  *before* the calibration gain is applied. A kind with a known analytical value pins it
  (`sine` references ≈ `-3.0103`); `pink_like` has no closed form and leaves it `null`, letting
  the engine record the measured value.
- **`calibration.applied_gain_db`** — the gain in dB the generator applied to move from the raw
  reference to the target. By construction `target = reference_rms_dbfs + applied_gain_db`.

In the capture manifest, the generator records a `GeneratorNormalization` carrying the **raw
pre-gain** `raw_rms_dbfs`, the delivered `target_rms_dbfs`, and the `normalization_gain_db`; the
delivered target is `raw_rms_dbfs + normalization_gain_db`. The generator
(`crates/fightbox-evidence/src/signal.rs`) measures the raw RMS first, computes the gain, applies
it, and **rejects** any target that would push a sample's absolute peak past 1.0 rather than
clipping silently.

**The generator normalization is not the physical source drive.** It only records how the
deterministic generator produces a buffer at a chosen program RMS. The scene-owned source drive
that maps a declared SPL to pre-propagation PCM is a separate, single gain chain defined by ADR
0002 and constructed only through
`fightbox_api::SceneCalibration::derive_source_drive`. The two must never be conflated or folded
into a second caller-supplied loudness gain. In the manifest, each source records the complete
chain — scene anchor, decoded `AssetAnalysis`, declared `ReferenceLevel`, derived `SourceDrive`,
the generator normalization separately when applicable, and a monitor gain / output transfer that
is explicit and distinct.

## Impulsive loop onsets

A composed loop may add a top-level `"onsets_s"` array. Its values are seconds from loop start,
must be strictly ascending, and must satisfy `0 <= onset < duration_s`. The composition tool emits
these timestamps from its deterministic placement cursor; it does not inspect or rewrite the WAV.
When the field is absent, the asset schedules no echo taps. A fixture source must independently set
`"impulsive": true` before an onset table is eligible for the echo sidecar.

## What this directory does NOT do

- **No delivered-ear-SPL claim.** Every descriptor carries the non-claim
  *"This descriptor makes no delivered-ear-SPL claim without output calibration."* A dBFS level
  is a PCM amplitude, not a sound-pressure level at the listener's ear.
- **The digital scene anchor is valid without an output transfer; delivered-ear SPL is not.** A
  fixture may declare a scene-level source level such as `SplAtOneMeter { db_spl: 85 }`. Mapping
  that 85 dB SPL to pre-propagation source PCM is a **digital scene calibration** owned by
  `SceneCalibration` (ADR 0002): it is well-defined without an output device, and the derived
  drive is what the renderer applies to the decoded asset PCM. With the Fightbox anchor (120 dB
  SPL ↔ −24 dBFS RMS at 1 m), an 85 dB SPL source playing a −20 dBFS RMS asset derives a −59 dBFS
  target source RMS and a −39 dB drive. A *delivered-ear* SPL claim is different and still
  requires a measured output-device/headphone transfer, which Phase A does not have (authority note
  §ρ). Evidence generation records the declared SPL, the decoded asset analysis, and the derived
  drive; it does not claim a calibrated sound-pressure level at the ear.
