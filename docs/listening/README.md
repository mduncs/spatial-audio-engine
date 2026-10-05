# Provisional S3 listening records

This directory holds the **provisional** Phase A listening-record contract for the S3
building-corner gate. It is the human-ears half of the S3 exit defined in the authority note
(§ν, §ο): a mechanically complete capture (bake → serialize → fresh-process reload → direct/path/
reflection stems → pathing-on/off comparison → metrics → manifest) is not an S3 pass until a
provisional listening record is completed and signed.

## What "provisional" means here

- **Human ears only.** Gate 0 — the ears-library self-validation against the v2.4 corruption corpus
  — does not exist yet. Extractor-based perceptual claims start counting in Phase B, after Gate 0
  passes. Until then a listening record is a human A/B/A judgment, not a measurement.
- **The template alone is never a pass.** Every record carries `requires_human_completion: true`
  and the non-claim `"Human completion is required; this template alone is not a pass."`
- **No delivered-ear-SPL claim.** Phase A has no measured output-device/headphone transfer, so a
  record reports the comparison and the result, not a calibrated ear SPL.

## Files

- `s3-listening-record.schema.json` — the strict Draft 2020-12 schema. It forbids unknown fields,
  pins `gate` to `S3`, forces `requires_human_completion: true`, constrains `comparison_order` to
  exactly `["pathing_on", "pathing_off"]`, and requires the human-required non-claim.
- `s3-listening-record.template.json` — a copy-from template. Fill in the `REPLACE_*` fields,
  record observations in the listener's own words, set `result`, and sign off.
- `validate.py` — a dependency-free validator. It enforces the two honest states the schema
  cannot express by itself: an `undecided` template may carry null hashes and an empty sign-off,
  while a `pass`/`fail` record must carry lowercase 64-hex fixture/bundle hashes, at least one
  non-placeholder observation, a nonempty signature, and a valid ISO-8601 sign-off date.

Run from the repository root:

```sh
jq empty docs/listening/s3-listening-record.schema.json docs/listening/s3-listening-record.template.json
python3 docs/listening/validate.py
git diff --check
```

## Required fields (summary)

- **listener identity** — `listener.listener_id` and free-text `notes`;
- **HRTF** — `hrtf.hrtf_set` and `hrtf.pretest_result` (provisional records leave the pretest as
  `not_run`);
- **fixture/bundle hashes** — `fixture_sha256` over the fixture JSON and `bundle_manifest_sha256`
  over the capture manifest, so a record is bound to an immutable fixture and a specific capture;
- **equipment** — headphones, output path, optional `monitor_gain_db` (monitor gain never changes
  simulated source power — authority note §λ);
- **comparison order** — at least two entries, default `["pathing_on", "pathing_off"]`;
- **observations** — free-text per stimulus;
- **result** — `undecided` (default), `pass`, or `fail`;
- **date** and **sign-off** — listener signature and date.

## Signing off

A record counts as a completed provisional S3 listening record only when:

1. `fixture_sha256` and `bundle_manifest_sha256` are populated from the actual capture;
2. `observations` describe a real A/B/A judgment, not placeholder text;
3. `result` is `pass` or `fail`;
4. `sign_off.listener_signed` and `sign_off.date_iso` are filled.

Even then, the Phase A S3 listening record is provisional: a pass here lets the mechanical S3 exit
close, but perceptual qualification against the full v2.4 percepts resumes after Gate 0 in Phase D.

## No delivered-ear-SPL claim; the digital scene anchor is a separate fact

A listening record is bound to a fixture by hash. The fixture's source may declare a scene-level
level such as `SplAtOneMeter { db_spl: 85 }`, and the asset it plays carries a delivered dBFS
program level (`target_rms_dbfs`, e.g. `-20`, with the generator's raw pre-gain `raw_rms_dbfs`
and `normalization_gain_db` recorded separately — see `fixtures/assets/README.md`).

Two facts this record deliberately keeps distinct:

- **The digital scene anchor is valid without an output transfer.** Mapping a declared
  `SplAtOneMeter` SPL to pre-propagation source PCM is a **digital scene calibration** owned by
  `SceneCalibration` (ADR 0002). With the Fightbox anchor it is well-defined without an output
  device: an 85 dB SPL source playing a −20 dBFS RMS asset derives a −59 dBFS target source RMS
  and a −39 dB drive, applied to the decoded PCM exactly once before propagation branches. This
  digital mapping does **not** require a measured output-device transfer.
- **A delivered-ear SPL still requires a measured output transfer, which Phase A does not have.**
  Phase A has no measured output-device/headphone transfer, so a listening record reports the
  comparison and the result, never a calibrated sound-pressure level at the ear. Monitor gain and
  the output-device transfer are downstream controls distinct from scene source power,
  propagation, and the source meter (authority note §λ; ADR 0002).
