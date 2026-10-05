# True moving-source reproduction

This isolates the existing Checkpoint MI-8 orbit rather than the gamma5 A-10 audition, whose motion is already authored into a recording. One 48-waypoint closed path runs at 30 m/s around a 190 m nominal radius, at 55 m elevation. Listener stays at the center, with the headless contract’s 1 mm initial movement. Default-off/restart-on-enable, source phase zero, 105 dB diagnostic monitoring convention. Existing private rotor media and retained megablock package/bake are reused unchanged.

The headless route shares the live Workbench source-position/velocity/height/safety update method, driven by the same sampled audio block as the listener. `replay.json` adds source trajectory metadata and per-sample position, velocity and forward direction. Publication block brackets remain observations, not sample-exact solver-adoption timestamps. Source paths wrap; the listener path holds its endpoint. `source_position_m` at the report root remains the original authored static field (null for moving sources); use the per-sample source position for motion.

A 12-second internal capture crosses multiple waypoint joins without crossing the rotor clip loop boundary (about 28.96 s). It verifies a true moving route and permits locating transition artifacts; it is not proof of perceptual quality, full-orbit behavior or fast-jet Doppler. The stationary center listener intentionally reduces range variation to make directional motion easier to isolate.

No audio-device defaults change. Unattended capture uses explicitly selected BlackHole 2ch with internal WAV evidence; loopback is unverified. Human previews require the already approved quiet peak/RMS ceiling and fades.

This fixture keeps altitude constant. Existing Workbench height selection flattens authored source altitude while its trajectory direction/velocity can retain a vertical component for varying-altitude paths; those paths are not qualified by this reproduction and are a separate follow-up.
