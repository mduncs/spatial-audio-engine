# Combat reference

Open `run-workbench-combat.command`, then press **Play scene**: quiet siren, circling MI-8, five artillery shots and an A-10 gun run. Sources start off; Stop rewinds. The performance ends at 80 s. WASD and looking remain available. Audio stays in ignored local `music` and `squad` directories.

| Scene seconds | Action |
|---|---|
| 0 / 4 | Siren / looping MI-8 orbit start |
| 18, 25, 32, 56, 64 | Artillery SW, S, SE, S, SW; SW also has the street shell/crack |
| 24 | A-10 starts at asset phase 28.136 s |
| 34, 34.18, 34.36, 34.54 | Four recorded impact onsets advance east along the south street |
| 35.823 / 39.5 | A-10 recorded cannon onset / close-pass peak, before propagation |
| 40 / 74 / 76 / 80 | Impacts / A-10 / rotor and final shell / siren stop |

The street package/bake retain 9,723 probes. Radius-8 m static coverage: SW `[102.5,102.5,1.5]` is 2.491 m from `[100.55,100.95,1.5]`; S is 1.551 m from `[292.55,100.95,1.5]`; SE coincides with `[548.55,108.95,1.5]`. The siren coincides with `[332.55,508.95,11.1]`, 1.5 m above a mesh-checked 9.6 m roof. The listener start is 2.771 m from `[428.55,484.95,1.5]`.

Aircraft reuse the 190 m/30 m/s MI-8 orbit and Checkpoint 10.452049 m/s A-10 racetrack at 70 m, above the mesh's 60 m highest roof. Radius-16 sky spheres at z=63 cover both; radius-8 floor spheres cover the impact line. Full paths sampled at <=0.5 m, including closure, have zero uncovered samples (1,536 impact, 2,400 rotor, 1,769 A-10). After reserving 0.25 m between samples, margins remain 3.750/3.348/2.506 m. Proof: `/path/to/spatial-audio/evidence/scenes-20260930/combat-coverage.json`. Seven ordinary sources plus the crack fit Desktop's eight detailed voices and the 16-slot limit.

Free-field calibration: `RMS dBFS = SPL - 144 + trim + 30 - 20 log10(distance)`. Existing anchors are unchanged; trims are audition gain. Full-route minima and WAV crests give:

| Source | SPL / trim dB | Min range m | RMS / peak dBFS at +30 |
|---|---:|---:|---:|
| Artillery SW / S / SE | 155 / -16 | 500 / 404 / 394 | -29/-9.7; -27.1/-7.9; -26.9/-7.7 |
| A-10 | 127 / -6 | 150 | -36.5 / -11.7 |
| Impact line | Palette 105 / +24 | 383 | -36.7 / -5.9 |
| MI-8 | Motion 105 / +12 | 81 | -35.2 / -17.4 |
| Siren | Megablock 118 / 0 | 97 | -35.8 / -18.9 |

These estimates precede HRTF, air, paths and returns; capture must qualify mixed peaks. Limiter and OutputSafety stay active. A-10 motion/Doppler already exists in the recording; its slow trajectory adds placement motion, with no dry fast-jet claim. The four impacts share one moving emitter, including their overlapping tails.

Capture the full performance from the worktree after the release build:

```sh
target/release/fightbox-workbench \
  --package /path/to/spatial-audio/evidence/megablock-seed1/megablock.fightbox \
  --baked /path/to/spatial-audio/evidence/astra-user-weak-street/road-sample-matrix/successor-path1500-vis40.baked \
  --fixture fixtures/city/combat-reference/fixture.json \
  --start-audio --device "BlackHole 2ch" --headless-replay --seconds 90 \
  --capture-root /path/to/spatial-audio/evidence/scenes-20260930/combat-full \
  --replay-monitor-gain-db 0
```

Measure input cue frames against `round(at_s * sample_rate)` and V1 arrivals against audible transients; propagation and SW ballistic delay shift heard arrivals. Check four impact spacings and limiter telemetry. Repeat at +30 in a distinct capture root for headroom; the 0 dB capture is 30 dB quieter. Sandbox listening/capture remains unperformed.
