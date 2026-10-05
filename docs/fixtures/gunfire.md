# Looping gun cracks

A static source with a `gunfire` block uses its looping muzzle WAV as the round
clock. Rounds are detected at load; the descriptor's `onsets_s` still describes
the composed bursts for the existing muzzle echo sidecar. Play starts the loop
and all its rounds automatically. Stop resets their phase. No separate shot
trigger is needed.

The source must start off, use `restart_on_enable: true`, and have zero playback
offset. Author `aim_point_m`, `muzzle_velocity_mps`, `supersonic_distance_m`,
`dispersion_m`, and `crack_peak_db_at_30_m`; `n_wave_ms_at_30_m` overrides the
existing duration anchor. The finite piecewise Mach planner admits only rounds
whose cone reaches the listener. Direct timing, Whitham level and duration use
each round's flight. The 250 ms activation pre-roll occurs once per Play.

`round_aim_offsets_m` optionally supplies a signed horizontal offset at the aim
plane for each detected round. Its length must match the round count. It repeats
with the actual WAV period; `dispersion_m` adds deterministic small variation.
These are fixed world trajectories: moving the listener changes the misses and
arrivals rather than aiming the gun after the listener.

`street_response` defaults to **true**. The existing crack companion uses a
**125 ms Steam convolution IR**, keeping the short facade response without a
second reverb. Set `false` for an explicit dry comparison. This cap applies only
to the gun companion's convolution and native simulated IR duration. Separate
inherited duration groups preserve the ordinary sources' budgets. The companion
retains the original rays, bounces and ambisonic order. Companion simulation updates at one fifth of the shared cadence
while geometry is unchanged; movement forces a refresh. With two active gun
companions whose central emission points are within 3 m, the retained async
route sums their independently delayed N-waves through one existing Steam
reflection effect and simulates that one street field. Both direct paths stay
independent. Existing effects remain preallocated for fallback when the points
separate or either gun stops; send changes fade over one block. The M2 field
is retained for the Heat pair, so its solo response is unchanged. This shared
field approximates the DShK reflection origin within 3 m. The muzzle source keeps
its own rays, bounces, duration, update schedule and reflections,
and artillery's existing `ballistic` crack stays dry. The direct N-wave anchor,
gain chain, proximity safety and final limiter are unchanged. Each gun still
uses one preallocated companion slot counted against `MAX_ACTIVE_SOURCES`.
The companion's bearing and reflection origin share one central tangent point;
its per-round direct timings, levels and durations remain separate.

The Heat default uses 30 authored rounds per gun. At its spawn, rounds 8, 18 and
28 miss by 3 m; the other 27 miss by 10–40 m. The target miss order is:

```text
18,28,12,36,22,32,16,3,26,38,14,24,34,20,30,
40,10,3,28,18,36,22,32,12,26,38,16,3,24,34 metres
```

The authored aim offsets differ for M2 and DShK because their muzzle positions
differ. The ready full Heat copy is
`../evidence/heat-ideas-20261002/crack/round3/heat-default.json`; its implementation
and listening/performance evidence are in `crack/REPORT-round3b.md`. The performance
gate must be checked there before treating this candidate as a signed-off default.
The WSL full-scene render uses the existing pinned `toms-diner` WAV for the off
music source, retaining all five scene sources without opening a device.

M2 uses 890 m/s ([General Dynamics M2A1, M33 ammunition](https://www.gd-ots.com/wp-content/uploads/2018/01/M2A1-50CAL-Heavy-Machine-Gun.pdf)).
DShK uses 840 m/s, the upper endpoint of the ammunition catalogue's 810–840 m/s
range ([DIO catalogue](https://military.ddns.net/Iran_weapon_industry/Section2.pdf)).
The existing 140 dB peak / 0.5 ms at 30 m anchor is the approximate 12.7 mm
measurement reading and Whitham rescaling documented in the fixtures and the
round-1 report ([Varnier & Sourgen, pp.44–45](https://euracoustics.org/documents/5/AiP_issue4.pdf)).
These are constant-speed finite flights, without drag, drop or bullet collision.

For matched offline cost and fidelity captures, `--replay-pin-full-quality`
requires `--headless-replay --null-output` and holds the quality governor at
Full. Callback timing, deadline counts, proximity protection and the output
limiter still run. The flag defaults off and is rejected for device output;
normal playback keeps adaptive quality. Use `heavy.sh --exclusive` around each
paced timing capture, with `/usr/bin/time -v` for process CPU accounting.
