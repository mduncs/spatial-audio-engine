# Astra urban scene

One device-free Workbench fixture that composes the retained corner crossing,
an authored artillery single-shot, and the true MI-8 orbit. It reuses the
`megablock-city-bake-v1` package/bake and the scene-local eight-bounce candidate
settings (`4096` rays, `8` bounces, `1.5 s` IR).

The listener walks the repeatable 25.2 m northbound corner route at 1.4 m/s.
All three sources start disabled and restart from their declared phase when
enabled. The artillery source uses the finite `astra-artillery-single` alias
with `impulsive: false`, preserving the ordinary recorded-shot path without
echo-sidecar semantics. The MI-8 retains its true 30 m/s orbit speed and is
raised to 70 m to clear the retained 58 m building roof; this is above the
63 m probe ceiling, so pathing coverage is not claimed. Both diagnostic sources
retain the constituent 105 dB monitoring reference; no physical ear-SPL claim
is made. The artillery and MI-8 are deliberately separate inputs: no claim is
made that the authored artillery recording or rotor recording is calibrated. Coordinates are synthetic local ENU, not surveyed map
data; eventual iOS/map presentation remains out of scope.

## Views and current playback boundary

The default main view remains first-person with its inset map. Select LIVE2D
above the main view for the enlarged north-up ground map, then choose a source.
Ground influence is a coarse geometry-constrained illustration, not solver rays
or measured energy. Elevated sources retain their position marker but do not
receive a misleading ground-confined influence field. Selected-source direct
visibility/path strength remain separately labeled solver observations.

The launcher deliberately opens no audio device. This is not an interactive
quiet-output guarantee: the existing editable Workbench monitor control is not
a final output ceiling. Do not equate the previously approved bounded WAV
preview level with arbitrary live source/distance/mix settings. Normal-output
testing so far uses bounded captured previews; a dedicated live quiet-output
route remains a follow-up.
