# Scenario 2: "Checkpoint Block" — Squad v8.1 rip in the megablock grid

Planning doc only. No repo or asset-drive changes. All asset paths are relative to
`/Volumes/External/GameAssets/Squad (v8.1) Sound, VO & Music/` (directory name contains spaces and parentheses — always quote).

## 1. Library taxonomy

19,107 .ogg / 617 MB, one root folder `Game/`. Counts by branch, with a verdict for this scene:

| Branch | Files | Verdict |
|---|---|---|
| `Game/Art/Items/<weapon>/Sounds/` | 4,949 | Handheld weapons: Fire (1p/3p), Reload, Deploy, foley for ~80 weapons (AKM, M240, RPG7, NLAW...). Gold for one-shots; foley irrelevant. |
| `Game/Audio/Character/` | 3,726 | Footsteps, breathing, gear, ragdoll. Irrelevant (engine is identity-free; no player body). |
| `Game/Audio/VO/` | 3,331 | Voice lines. Irrelevant. |
| `Game/Vehicles/<vehicle>/Sounds/` | 2,639 | ~70 vehicles, each with `Engine/EXT/Idle|Low|Mid|High`, `INT`, Turret, Weapons, Damage. **The single richest vein**: honest exterior idle loops for tracked and wheeled vehicles, emplacement fire, helicopter rotor loops at Close/Mid/Far perspectives. |
| `Game/Audio/Impacts/` | 2,134 | Bullethits by material, `Bulletpassbys/Cracks` (supersonic crack library!), `Explosions/Artillery|JDAM` at Close/Mid/Far, shells, shockwaves. Gold for the ballistic event system. |
| `Game/Audio/Reverb/` | 915 | Baked reverbs/tails. **Avoid** — Steam Audio provides the acoustics; baked reverb double-dips. |
| `Game/Audio/Ambient/` | 549 | Gold: `Industrial/Generators`, `Fires`, `Villages`, `Wind`, `Animals`, `Church/Bell`, `Humans`. |
| `Game/Audio/Deployables/` | 229 | FOB radio static loop, razorwire, hesco, sandbags — checkpoint set dressing. |
| `Game/Audio/Amphibious/` | 195 | Water/boats. Not for this scene. |
| `Game/Audio/Vehicles/` | 141 | Cross-vehicle FX (TracksFX, SmokeGenerator). Secondary. |
| `Game/UI/`, `Game/Audio/Music/`, `Game/Blueprints/`, `Game/Maps/`, `Game/Audio/Weapons/Zoom` | ~300 | Irrelevant. |

Naming conventions that matter: `_ext`/`_int` = exterior/interior mic perspective (always take `ext`); `1p`/`3p` = first/third person (always take `3p`); `Close/Mid/Far` = baked distance perspective (prefer the closest/driest — the engine does its own distance work); `_loop` suffix = authored seamless loop; numbered variants `_01.._NN` = round-robin pools.

## 2. Scene concept

**A checkpoint the morning after.** One army holds the central intersection of the megablock: an M1A2 Abrams idling at the checkpoint with a field radio hissing and camo netting flapping beside it. One block north, a Ural supply truck idles — wheeled diesel clatter contrasting the Abrams turbine whine across the length of one street. A diesel generator hums on the parallel street one block west, fully occluded until a corner reveal. Evidence of the night's fighting: a car burns mid-block on the western approach, and a large building fire rages two blocks east, audible as a distant roar down the street canyon only when the canyon lines up. An Mi-8 circles the district above the rooftops. On demand (spacebar), an unseen M2 .50 cal two blocks south fires north up the listener's street — supersonic crack overhead, muzzle report arriving late.

Perceptual walk design (grid facts from the megablock fixture: 95 m block pitch, street centerlines at x,y ∈ {7.5, 102.5, 197.5, 292.5, 387.5, 482.5, 577.5}, rooftops 26–38 m, probe volume to 63 m, street half-width ≈ 7.5 m so lateral offsets ≤ 5 m stay in the street):

- **Leg 1 (west → center, y=292.5):** start at [197.5, 292.5] in relative quiet; pass the burning car at close range mid-block; the Abrams grows from a muffled canyon rumble to a dominant near-field source; radio and canvas detail resolve only in the last 20 m — distance layering from one loud anchor.
- **Leg 2 (center → north, x=292.5):** Abrams behind, Ural ahead — tracked-turbine vs wheeled-diesel crossfade along 95 m. The .50 cal event fires up this exact street: crack-then-blast timing is maximally legible here.
- **Leg 3 (north → west, y=387.5):** at [292.5, 387.5] the eastern canyon lines up and the distant building fire pours in (190 m line-of-sight), then shuts off as the listener moves on — canyon gating. Approaching [197.5, 387.5] the generator gets its corner reveal.
- **Leg 4 (west → south, x=197.5, back to start):** walk directly past the generator; Abrams returns as diffracted-around-the-block energy. Mi-8 crosses overhead throughout, alternately open-sky and roof-shadowed.

Listener: closed-loop trajectory [197.5, 292.5, 1.5] → [292.5, 292.5, 1.5] → [292.5, 387.5, 1.5] → [197.5, 387.5, 1.5] → back, at 1.5 m/s (true walking pace — md's call, replacing the 6 m/s megablock convention).

## 3. Source table

8 sources — deliberately at the workbench schema's `maxItems: 8` cap — plus 1 ballistic event at its `maxItems: 1` cap (caps confirmed by md as staying). Bench alternates below.

SPL@1m figures are now reference-backed where literature exists — see the **Level references** section for citations, back-solves, and which figures are honest estimates. Figures that moved from the original draft: Mi-8 **118 → 126 (+8 dB, flagged for md)**; Ural 87 → 84; Abrams 92 → 90.

| # | Name | Asset path(s) | Position [x,y,z] & why | Shape | SPL@1m | ImpulseClass | Loop? |
|---|---|---|---|---|---|---|---|
| 1 | `abrams-idle-checkpoint` | `Game/Vehicles/M1A2/Sounds/Engine/EXT/Idle/abrams_engine_ext_idle_01.ogg` (pool: `_idle2`) | [292.5, 292.5, 1.5] — central intersection, the scene's anchor; same spot as scenario 1's Tom's Diner for direct A/B | line_segment 8 m (hull ≈ 9.8 m; should read BIG) | 90 dB **(estimated — no published exterior-idle measurement exists; ref E1)** | None | loop |
| 2 | `ural-truck-idle-north` | `Game/Vehicles/Ural4320/Sounds/Engine/EXT/Idle/Ural2_engine_ext_idle_01.ogg` | [292.5, 387.5, 1.5] — one block north of checkpoint; wheeled-diesel counterpole to the Abrams along leg 2 | line_segment 6 m | 84 dB (ref E2, measured idle series back-solved to 1 m) | None | loop |
| 3 | `generator-parallel-street` | `Game/Audio/Ambient/Industrial/Generators/ambient_generator_diesel_01.ogg` | [197.5, 340.0, 1.5] — mid-block on the parallel street one block west; fully occluded from legs 1–2, corner-reveals on leg 3, passed close on leg 4 | line_segment 3 m | 84 dB (ref E3, CHPPM: MEP TQGs 80–87 dBA at operator panel) | None | loop |
| 4 | `burning-car-west-leg` | `Game/Audio/Ambient/Fires/fire_car_01.ogg` (pool: `fire_car_02`) | [245.0, 292.5, 1.5] — mid-block on leg 1, walked past at ~5 m; first landmark | line_segment 4 m (car length) | 85 dB **(estimated — no findable fire-SPL measurement; ref E4)** | None | loop |
| 5 | `building-fire-far-east` | `Game/Audio/Ambient/Fires/fire_building_large_01.ogg` | [482.5, 387.5, 8.0] — two blocks east of the Ural corner, raised into a facade; only line-of-sight down the y=387.5 canyon — mid-distance ambience that gates on/off with canyon alignment | line_segment 12 m (structure fire reads huge) | 96 dB **(estimated — same gap as the car fire; ref E4)** | None | loop |
| 6 | `fob-radio-checkpoint` | `Game/Audio/Deployables/FOB/Radios/us_fob_radio_static_noise_loop.ogg` | [287.0, 296.5, 1.2] — sandbag corner of the checkpoint, ~6 m off the Abrams; near-field detail that only resolves in the last meters | point | 70 dB (ref E5, anchored to loud-speech level at 1 m) | None | loop |
| 7 | `camo-net-flap-checkpoint` | `Game/Audio/Ambient/Wind/Tent/camo_tent_flapping_01.ogg` | [298.0, 288.5, 3.0] — opposite corner of the checkpoint, elevated on a frame; with #6 it stereo-brackets the intersection | point (option: stereo_image 4 m) | 65 dB **(estimated — no literature on canvas/net flap SPL exists; ref E6)** | None | loop |
| 8 | `mi8-orbit` | `Game/Vehicles/MI8/Sounds/Engine/Rotor/Close/mi8_engine_idle_close_01.ogg` — **needs ear check**, see prep notes | trajectory square [102.5, 102.5, 55] → [482.5, 102.5, 55] → [482.5, 482.5, 55] → [102.5, 482.5, 55], 30 m/s — above the 26–38 m rooftops, inside the 63 m probe ceiling; alternately open-sky and roof-shadowed | point (moving) | **126 dB (was 118 — moved +8 dB, flagged)** (ref E7, measured 103.7–108.3 dBA at 10 m, back-solved) | None | loop |

Occlusion: the consumed schema has **no per-source `occlusion_samples`** — it is global at `simulation.direct.occlusion_samples`. Recommend 64 (matches megablock), sized for the large extents (#1, #5). If per-source sampling lands later: 64 for the line extents, 16 for the points.

### Ballistic shot event (spacebar)

| Field | Value | Why |
|---|---|---|
| id | `checkpoint-m2-shot` | |
| muzzle_m | [292.5, 102.5, 2.0] | Unseen M2 emplacement two blocks south of the checkpoint |
| direction_enu | [0, 1, 0] | Fires north up the x=292.5 street — the listener's leg-2 street, so crack-vs-blast geometry is maximally legible |
| mach | 2.6 | .50 BMG ≈ 890 m/s / 343 m/s |
| blast asset | `Game/Vehicles/Emplaced50cal/Sounds/Fire/3p/AUTO/Default/Initial/m2_fire_loop_initial_3p_default_01.ogg` (pool `_01.._NN`; single report, 0.115 s, mono 48 k — ideal trigger sample) | |
| crack asset | **engine-synthesized N-wave** — the frozen signed contract; the Squad sample contribution is the blast only. The `Crack_Hiss` 15-variant pool (`Game/Audio/Impacts/Bulletpassbys/Cracks/Crack_Hiss/passby_crack_hiss_01..15.ogg`) is documented here as a future option, not part of v1 | |
| blast_spl_at_one_meter_db | 155 | ref E8: 155 dBP at gunner's position (HPRC/CHAMP); CHPPM gives 153 dBP at HMMWV gunner position — see Level references |
| crack_over_blast_db_at_30_m | 3.0 | Carry over the megablock-validated ratio |
| ImpulseClass | ArtilleryThunder (or a lighter impulsive class if one exists for small-arms — see open questions) | |

### Bench (swap-ins, since the schema caps sources at 8)

- `Game/Vehicles/T72B3/Sounds/Engine/EXT/Idle/t72_engine_idle_ext_01.ogg` — swap for #1 to get diesel-tracked instead of turbine-tracked (mono 48 k, 10.75 s).
- `Game/Audio/Ambient/Church/Bell/church_bell_01.ogg` — rooftop bell, direct callback to scenario 1's bell (stereo 48 k, 22.7 s).
- `Game/Audio/Ambient/Villages/town_abandoned_ambience_01.ogg` — 2-minute stereo bed; would want `stereo_image` extent ~40 m elevated over the walk (stereo 22.05 k).
- `Game/Audio/Ambient/Industrial/Electricity/electricity_humming_01.ogg` — substation hum, another occludable point drone.
- Event alternates: `Game/Audio/Impacts/Explosions/Artillery/Close/artillery_impact_close_01.ogg` (ArtilleryThunder shell impact), `Game/Audio/Impacts/Bulletpassbys/TankShell/120mm/120mm_flyby_01.ogg`, `Game/Vehicles/Emplaced_M1937mortar/Sounds/Incoming/mortar_shell_incoming_close_01.ogg`.

## 3b. Level references

Propagation convention for back-solves: spherical spreading, −6 dB per distance doubling, i.e. SPL@1m = SPL@r + 20·log₁₀(r). Caveat for large/extended sources (rotor disks, vehicle hulls, building fires): 1 m is inside the near field, so the back-solved "SPL@1m" is not what a meter would read there — it is the far-field-consistent anchor, which is exactly what the engine's distance chain needs (the listener is never at 1 m from these sources in this scene). A-weighted (dBA) and peak (dBP) figures are carried as-is into `SplAtOneMeter`; the M2 figure is a *peak* level, appropriate for an impulse event.

- **E1 — M1A2 Abrams idle, exterior: 90 dB @1m, ESTIMATED.** No published exterior-idle measurement was found (searched CHPPM/APHC equipment tables, DTIC, NATO STO, and installation EIS literature — everything published is crew-position or moving). Anchors: CHPPM lists Abrams *interior* steady levels of 96–117 dBA when moving ([CHPPM Noise Levels of Common Army Equipment, via pdf4pro mirror](https://pdf4pro.com/view/noise-levels-of-common-army-prevention-equipment-72c3fa.html)); a measured heavy-diesel idle is ~84 dB at 1 m (E2); the AGT1500 turbine is documented as notably quiet at idle (Honeywell AGT1500 brochure, qualitative). 90 dB places the turbine idle above a diesel truck but below "moving" interior levels. Needs md's ear at mix time.
- **E2 — Ural-4320 idle: 84 dB @1m, moderate confidence.** Community/advocacy-compiled roadside measurements of an idling heavy diesel truck: 84 dB beside the exhaust (~1 m), 64 dB at 30 ft (9.1 m) ([CEDS noise page](https://ceds.org/noise/)); the 9.1 m reading back-solves to 64 + 20·log₁₀(9.1) ≈ 83 dB @1m, internally consistent with the near reading. Not peer-reviewed, but two distances that agree under spherical spreading. The federal stationary run-up limit (40 CFR 202, 85 dBA at 15.2 m, governed rpm) is an upper bound, not an idle figure. Kept 84 (was 87, −3 dB).
- **E3 — field diesel generator: 84 dB @1m, solid.** CHPPM/APHC equipment tables: MEP-802A 5 kW Tactical Quiet Generator = **80 dBA at the operator panel** (~1 m) at rated load; MEP-803A 10 kW = 81 dBA; MEP-804A 15 kW = 84 dBA; MEP-806A 60 kW = 87 dBA ([CHPPM tables](https://pdf4pro.com/view/noise-levels-of-common-army-prevention-equipment-72c3fa.html)). Picked 84 = mid-size (15 kW-class) set, matching the beefy character the asset name implies; the Squad asset also sounds unsilenced (TQGs are the *quiet* program), so 84 is if anything conservative. Original estimate unchanged.
- **E4 — car fire 85 dB / building fire 96 dB @1m, ESTIMATED.** Plainly: no findable SPL measurement of open vehicle or structure fires exists in the accessible literature. Fireground noise studies (NIOSH 2013-142; Neitzel et al. 2013, [PubMed 23339379](https://pubmed.ncbi.nlm.nih.gov/23339379/)) measure *equipment* (saws, pumps, sirens ≥85 dBA), never the fire itself; combustion-acoustics OASPLs (113–119 dB) are confined lab burners, not applicable. The 85/96 pair preserves a plausible 11 dB size ordering. These two are the least-grounded levels in the scene — tune by ear against the calibrated chain.
- **E5 — FOB radio static: 70 dB @1m, anchored analogy.** A field radio speaker is set so speech is intelligible outdoors, i.e. at loud-speech level: Pearsons et al., *Speech Levels in Various Noise Environments* (EPA-600/1-77-025, 1977) puts loud speech at ~66–72 dB at 1 m. Static noise rides the same volume setting → 70 dB. Solid classic reference, applied by analogy.
- **E6 — camo-net flap: 65 dB @1m, ESTIMATED.** No literature on canvas/net flap SPL exists (flag aeroacoustics papers are lab-scale, unreported in field SPL). Set just below the radio so it reads as texture, not signal.
- **E7 — Mi-8 orbit: 126 dB @1m, solid, MOVED +8 dB.** Aravindakshan, Aravind & Vyawahare (2002), *Analysis of on-ground and in-flight sound levels produced by Chetak and Pratap helicopters*, Indian J Aerospace Med — Pratap (= Mi-8) ground run at max rpm with rotors engaged, measured at 8 positions on a 10 m radius: **103.7–108.3 dBA** ([article](https://indjaerospacemed.com/analysis-of-on-ground-and-in-flight-sound-levels-produced-by-chetak-and-pratap-helicopters/)). Median ≈ 106 dBA @10 m → 106 + 20·log₁₀(10) = **126 dB @1m** (far-field-consistent anchor; near-field caveat above applies — the rotor disk is 21 m across). Corroboration: OSHA's heliport eTool notes ≥105 dB "in all operating conditions" and hazard >85 dBA beyond 100 ft ([OSHA](https://www.osha.gov/etools/hospitals/heliport/noise-communication)). Original 118 was an 8 dB under-guess — **this changes scene balance; md should see it**. At the 53 m minimum slant range the helo now delivers ~91 dB, correctly dominant during overflight.
- **E8 — M2 .50 cal blast: 155 dBP @1m-equivalent, solid.** HPRC/CHAMP (Uniformed Services University), *Hearing Protection 101* infographic (2019): **M2 .50 cal machine gun, 155 dBP at gunner's position** ([PDF](https://www.hprc-online.org/sites/default/files/document/HPRC_PF_GR_Hearing%20Loss%20Infographic_508_050819.pdf)); CHPPM equipment tables give 153 dBP at the HMMWV gunner position. The gunner sits ~1 m behind the muzzle, so 155 needs no back-solve. Kept 155 (the dismounted-gun figure, and identical to the megablock artillery precedent). Note it is a peak level — correct semantics for the event system's impulse.

## 4. Asset prep notes

Spot-check results (ffprobe):

| File | Rate | Ch | Dur | Prep |
|---|---|---|---|---|
| abrams_engine_ext_idle_01 | 22,050 | 1 | 17.7 s | **resample → 48 k**; 22 k on the scene anchor is the biggest quality risk — audition idle vs idle2, consider T72 swap if thin |
| Ural2_engine_ext_idle_01 | 48,000 | 1 | 49.1 s | none |
| ambient_generator_diesel_01 | 48,000 | 1 | 25.5 s | none |
| fire_car_01 | 22,050 | 2 | 9.0 s | mono-fold + resample; short loop — listen for seam |
| fire_building_large_01 | (not probed) | | | probe + likely mono-fold |
| us_fob_radio_static_noise_loop | 48,000 | 1 | 66.4 s | none; `_loop` in name |
| camo_tent_flapping_01 | 48,000 | 2 | 32.6 s | mono-fold (or keep stereo if using stereo_image) |
| town_abandoned_ambience_01 | 22,050 | 2 | 123.7 s | bench only; mono-fold + resample |
| church_bell_01 | 48,000 | 2 | 22.7 s | bench only; mono-fold |
| t72_engine_idle_ext_01 | 48,000 | 1 | 10.75 s | none |
| m2_fire_loop_initial_3p_default_01 | 48,000 | 1 | 0.115 s | none — clean trigger sample |
| artillery_impact_close_01 | 44,100 | 2 | 8.3 s | bench; resample + mono-fold; 8 s baked "close" tail partly duplicates engine reverb |

General findings:

- The rip is mixed-rate: mostly 48 k mono for vehicle/weapon exteriors, but ambience and some older vehicle content is 22.05 k and/or stereo. Assume a normalization pass to the engine's reference format (scenario 1 reference is `toms-diner-48k-mono.wav`, 48 k mono WAV): **decode ogg → mono-fold where stereo → resample to 48 k**.
- Loop points: oggs carry no usable loop metadata after ripping; UE cue assets held that. Files named `_loop` are safe bets for seamlessness; the vehicle `Idle` files are almost certainly authored seamless (they loop in-game) but **verify each by butt-splice listen; apply short equal-power crossfade looping as the default fallback**. Highest seam risk: fire_car_01 (9 s).
- Baked perspective: many pools exist at Close/Mid/Far with baked air absorption and reverb. Policy proposal: **always take the closest/driest variant and let the calibrated chain do distance** — using "Far" assets would double-apply distance character. This is why the Mi-8 pick is flagged: its exterior loops are all named `idle` at Close/Mid/Far/INT perspectives, and whether "Close idle" reads as flyable rotor wash or ground-idle whine needs an ear.
- Avoid the entire `Game/Audio/Reverb/` branch (915 files) for the same double-dip reason.
- **Ballistic crack: no sample prep.** The M2 event's crack remains the engine's synthesized N-wave — that is the frozen signed contract. The Squad contribution to the event is the blast sample only. The `Crack_Hiss` pool is catalogued in the event table as a documented future option should the contract ever reopen; nothing in v1 decodes or ships those files.

## 5. Open questions for md

Settled by md (2026-08): caps stay at 8 sources + 1 event; the 4-leg closed loop at 1.5 m/s is adopted; every level is now reference-backed per the Level references section, with honest ESTIMATED flags where literature doesn't exist.

1. **Where does ImpulseClass live?** It isn't in the workbench fixture schema — presumably asset-registry metadata (as with `artillery-impact`). Confirm where to declare it, and whether a small-arms impulsive class exists or the M2 event just rides ArtilleryThunder.
2. **Ear checks needed:** (a) Mi-8 Close vs Mid rotor character (flight vs ground idle); (b) Abrams 22 k idle — good enough upsampled, or swap to the 48 k T72 and accept diesel-tracked instead of turbine; (c) fire_car_01 loop seam; (d) generator loop seam; (e) the two fire levels (E4) are the least-grounded in the scene — tune by ear.
3. **Dry-asset policy:** OK to standardize on closest/driest variants everywhere and ban baked Far/Reverb content, per above?
4. **Mi-8 at 126 dB:** the researched figure came in 8 dB hotter than the draft. Accept the measured level (helo correctly dominates during overflight), or artistically trim 3–6 dB if it swamps the street layer?
