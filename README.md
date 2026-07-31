# Spatial Audio Engine

A city you can hear.

This is a toy about walking through urban space with your ears: you move a listener
through real or synthesized city geometry wearing ordinary headphones, and the sound
behaves as though the buildings are actually there. A diner's jukebox localizes down
the block and holds still while you turn your head. A wall steps in front of it and the
sound dims and dulls. Around the corner, the music doesn't vanish — energy bends along
the street and arrives muffled, from the direction of the opening. An impact three
blocks away rumbles off facades before it reaches you; the same impact in an open plaza
sounds dry and far. Distance costs level, air, and time.

None of that is scripted. The engine simulates it from geometry, materials, and motion.

## What it is

A Rust engine around [Steam Audio](https://valvesoftware.github.io/steam-audio/) 4.8.1
that turns city geometry plus source and listener motion into binaural audio:

- **Direct sound** with distance attenuation, air absorption, and ray/volumetric
  occlusion against building meshes.
- **Baked propagation paths** — sound diffracting around corners and along streets
  through a precomputed probe network, so occluded sources arrive from the geometry's
  actual openings instead of through walls.
- **Reflections** — real-time convolution against the surrounding facades, so a street
  canyon, an open plaza, and a courtyard read differently without looking at the screen.
- **A performance governor** that degrades quality by audibility, never by teleporting
  or clicking: under load the mix stays continuous and sources fade rather than pop.
- **Deterministic city tooling** — a compiler from GeoJSON to reproducible world
  packages, a synthetic Manhattan-grid generator for stress fixtures, and a bake
  pipeline whose artifacts are content-hashed end to end.
- **A desktop workbench** — walk the city with mouse and keyboard, teleport sources
  between street level and above the rooftops, watch a solid-shaded map of what's
  blocking what, and capture evidence of what you heard.

The engine core is map-neutral and host-neutral: the same crates drive the headless
renderer, the desktop workbench, and an iOS host. Steam Audio is the first propagation
backend, behind a frozen seam, not the identity of the project.

## Try it

Portable build (no SDK required):

```sh
cargo test --workspace
cargo run -p fightbox-cli -- status
```

Acquire the Steam Audio SDK (verified download into ignored `.cache/`):

```sh
scripts/acquire-steam-audio.sh
export STEAM_AUDIO_SDK_DIR="$PWD/.cache/steam-audio/steamaudio-4.8.1"
cargo test --workspace --features linked-sdk
```

Synthesize, compile, and bake a city, then walk it:

```sh
mkdir -p ~/fightbox-runs/demo
cargo run --release -p fightbox-cli --features linked-sdk -- \
  city synth --seed 1 --blocks 6x6 --output ~/fightbox-runs/demo/city.geojson
cargo run --release -p fightbox-cli --features linked-sdk -- \
  city compile --geojson ~/fightbox-runs/demo/city.geojson --output ~/fightbox-runs/demo/city.fightbox
cargo run --release -p fightbox-cli --features linked-sdk -- \
  city bake --package ~/fightbox-runs/demo/city.fightbox --output ~/fightbox-runs/demo/city.baked \
  --path-range-m 600 --visibility-range-m 20 --probe-spacing-m 8 --probe-ceiling-m 63 --bake-threads 10
cargo run --release -p fightbox-workbench --features linked-sdk,live-output -- \
  --package ~/fightbox-runs/demo/city.fightbox --baked ~/fightbox-runs/demo/city.baked \
  --fixture fixtures/city/megablock/fixture.json
```

The bake flags trade coverage for bake time; the defaults bake a small test block in
seconds, while the settings above give a 585 m city-block grid full around-corner
coverage in under a minute on an M4.

## How it's put together

- `crates/fightbox-api` — SDK-neutral domain types: ENU coordinates, sources,
  listeners, simulation configuration.
- `crates/fightbox-runtime` — the real-time spine: immutable snapshots, the audio
  callback, command queues, telemetry, and the frozen backend seam.
- `crates/fightbox-steam-audio` — the contained Steam Audio FFI/RAII layer and the
  retained multi-source simulation (direct, pathing, reflections, smoothing, governor).
- `crates/fightbox-world` — GeoJSON city compilation, materials, deterministic meshes.
- `tools/fightbox-cli` — headless rendering, city tooling, bakes, evidence sweeps.
- `tools/fightbox-workbench` — the interactive desktop walkthrough.
- `platforms/ios` — the iOS host app scaffold.

Everything audible is measured: rendered stems, occlusion curves, path energies, and
callback deadlines are captured as machine-readable evidence, and the perceptual claims
(does the corner *sound* like a corner?) are qualified by structured listening sessions
rather than vibes.

## What it deliberately does not model

Exact computational aeroacoustics, nonlinear blast physics, weather refraction and wind
shadow, centimeter-scale geometric detail, continuously deforming geometry with live
rebakes, or calibrated absolute SPL at the eardrum. The goal is perceptually
convincing, physically coherent, measurable — and honest about which phenomena are
simulated versus suggested.
