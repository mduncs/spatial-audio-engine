# Single-shot artillery diagnostic

Enable the source once to hear one complete impact and its decay; disable/re-enable to re-arm. Default startup is silent. This scene makes body and later returns inspectable without a new shot every three seconds.

The distinct `astra-artillery-single` descriptor references the same existing, hash-verified private WAV as `artillery-impact`, with identical normalization and `loop: false`. It does not copy audio. The ordinary artillery source deliberately resets its cursor every three seconds; this diagnostic bypasses that asset-ID-specific behavior without changing it. The raw recording has very weak remaining energy after three seconds (about 0.00746% of total energy), so its truncation is not established as the cause of earlier body/scale complaints.

Use the retained `../spatial-audio-engine-runs/megablock-seed1/megablock.fightbox` package and corresponding `megablock.baked`. Source position and six-meter line extent match original megablock artillery. Listener holds approximately 46 m away in the existing corner control position (1 mm initial movement satisfies the headless route contract). Source reference is the diagnostic 105 dB monitoring convention, not the original 155 dB source reference; do not compare unadjusted loudness to that scene or infer delivered-ear SPL.

`impulsive: false` matches original megablock artillery: no discrete echo-sidecar onset table or ballistic impulse-class activation. Steam direct/path/convolution rendering remains the live Workbench route. The recorded impact itself contains decay; this is not an ideal dry impulse and does not by itself separate recorded tails from simulated ones.

`street-path-candidate.json` differs: its source is `impulsive: true`, so the Workbench plans discrete late echoes from the package mesh and triggers them with each shot (the asset's single `onsets_s: [0.0]`). It never adds the bare source-to-listener street route, which baked pathing already renders; see that fixture's `expected` text for the path rules and non-claims.

A 12-second internal headless capture is sufficient to include the ~6-second recorded impact, simulation decay, and silence. Use explicit `--start-audio --device 'BlackHole 2ch'` for unattended capture; its loopback remains unverified, so inspect `capture.wav`, not a presumed audible result. Any normal-output preview must respect the already approved measured playback ceiling and fades.
