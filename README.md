## Hello, human

I want a 3d audio simulation that can be reused for a lot of the toys I want to make. Audio elevates production value, but it used to be expensive, agents make it cheap qed we get value for a bargain! Or so I say, feel free to rip apart as needed for your own projects. Original project goal = remake the Heat shootout scene, and we snowballed into all of this.

---

*The rest of this README was written by an AI model (Claude Opus 5.5) from the code in this repository.*

### Why this is public

Spatial Audio Engine is personal software. I built it with AI coding agents for my own use: to place sounds, including my own music, at real spots in a city and hear them shaped by the surrounding buildings as I walk around with headphones. I'm publishing it because there's no reason not to.

Consider it a courtesy. If you, or an agent working for you, are building something similar, there may be something useful here. It isn't supported, and I won't be testing it on other setups or promising fixes. The tokens have been spent; this is me giving some back.

---

# Spatial Audio Engine

A Rust engine that puts sound sources into real or synthetic city geometry and renders what a moving listener would hear, as binaural audio for ordinary headphones. Buildings block, bend and reflect the sound. A source around a corner arrives from the street opening rather than through the wall, and a narrow street sounds different from an open plaza. Propagation runs on Valve's [Steam Audio](https://valvesoftware.github.io/steam-audio/) 4.8.1. Map import, scenes, distance and air modelling, output safety and the desktop app are this repository's own code. Crates and binaries use the internal name **Fightbox**.

## Features

**Cities from maps.** One command, `fightbox city build`, takes a place name, a point and radius, or a bounding box. It fetches OpenStreetMap buildings, extrudes them to their heights, bakes the probe network used to route sound around them, and writes a ready-to-open scene. In Chicago (Cook County) it also uses LiDAR roof heights and county assessor building materials. Compiled world packages are reproducible byte for byte.

**How it sounds.** Each source gets direct sound with distance, air absorption and occlusion, sound routed around buildings through baked probes, and real-time reflections off the facades, all rendered binaurally. Travel time follows path length, so moving sources produce Doppler naturally. Humidity and temperature presets change the high-frequency loss with distance. Distant events, out to about 10 km, travel a separate long-range path and arrive late, the way thunder or artillery does. Gunshots get a synthesised supersonic crack.

**Built for real time.** Simulation runs on its own thread and hands results to the audio callback without locks or allocation. Under load, a quality governor steps reflection and path detail down instead of glitching, keeping full detail for the most audible sources. All output passes one calibrated gain chain and a true-peak limiter that cannot be switched off.

**The Workbench.** A macOS desktop app for walking a city in first person (WASD, Shift to sprint, drag to look) or watching it on a map. Sounds can be dragged to new spots and saved back to the scene. Scenes are JSON files with moving sources and a timed cue list. A dropped song file, any app's audio or the whole system's audio can play from a speaker in the city. The app also draws sound as ripples and arrival pulses, follows AirPods head tracking, and renders scenes offline to binaural or ambisonic WAV.

**Extras.** A companion macOS app shows the live scene over Apple Maps in 3D. A C ABI and a Swift package let other hosts, including an iPhone app, embed the engine.

## Requirements

- macOS, developed on Apple Silicon. The City Map and AirPods helpers need macOS 14, and app audio capture needs macOS 14.2.
- Rust 1.91.1 (pinned in `rust-toolchain.toml`).
- The Steam Audio 4.8.1 SDK, fetched by a script (not vendored).
- A git clone, since the CLI build records the commit.
- Headphones.

## Build and run

```sh
cargo test --workspace --exclude fightbox-ffi        # portable, no SDK
scripts/acquire-steam-audio.sh
export STEAM_AUDIO_SDK_DIR="$PWD/.cache/steam-audio/steamaudio-4.8.1/steamaudio"
cargo build --release -p fightbox-cli -p fightbox-workbench --features linked-sdk,live-output
```

Build a scene from a map area (output must be outside the repository):

```sh
target/release/fightbox city build --center 41.8728,-87.6290 --radius-m 250 \
  --graded --output "$HOME/fightbox-scenes/printers-row"
```

This writes a `run-<place>.command` launcher that opens the Workbench on the scene. City scenes refer to audio files that are not in the repository, so a fresh clone can't play one yet.

Or have your agent do it.

## How it works

Map data is compiled into a world package (mesh plus materials) and baked into a probe network. A scene file places sources in that world. A simulation thread asks Steam Audio for direct, routed and reflected sound; the audio thread applies gain, air filtering, delay and HRTF, then limits the output. All Steam Audio calls live in one crate behind a fixed interface.

## Data and privacy

Only map-based `city build` goes online: Nominatim, the Overpass API, and, for Chicago, Illinois LiDAR and Cook County open data. Responses are cached in the output folder. The map companion loads Apple Maps tiles over a loopback-only link to the Workbench. There is no telemetry. Recordings go to an `evidence` folder beside the repository. Saving a scene rewrites source positions in its JSON file.

## Limitations

- **No audio included.** Most bundled scenes, and the ones `city build` writes, refer to WAV files that are not in the repository, and the Workbench won't start without them. Scene sources can point at a local song instead (`program_file`).
- The `run-*.command` launchers and some scripts point at paths on the author's machines.
- Geometry is simplified: flat ground, extruded footprints, and static buildings that need a re-bake after any change. LiDAR heights and materials only cover Cook County.
- Wind and weather refraction are not modelled, and corner diffraction uses Steam Audio's default model, which the project's notes call weaker than a real building edge.
- Song input, app audio capture and head tracking are macOS only. On Linux the Workbench can only render headless.
- The iPhone app builds but has never run on a phone.
- `LICENSE` says MIT, while the Cargo manifests say Apache-2.0.

## Keywords

spatial audio, binaural rendering, HRTF, Steam Audio, Rust, acoustic simulation, sound propagation, occlusion, diffraction, reflections, ambisonics, Doppler, ISO 9613-1, urban acoustics, OpenStreetMap, GeoJSON, LiDAR, probe baking, egui, wgpu, cpal, Core Audio process tap, AirPods head tracking, MapKit, auralization, acoustic digital twin, walkable soundscape
