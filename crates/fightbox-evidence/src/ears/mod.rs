//! Offline perceptual evidence over plain deinterleaved PCM.
//!
//! This module is a schema-free port of the reusable DSP ideas in the V2
//! `ssim-ears` donor. The port was made from spatial-audio-sim-v2 commit
//! `fb8d1a3ece9ec577664c13891f29e4b96c31b33a`. In particular, the correlation,
//! comb, modulation, transient, ITD/ILD, IACC/width, and Schroeder extractors
//! retain the donor's f64 accumulation and deterministic offline character.
//! V2 `Capture`, session, sidecar, trajectory, and path-record types were
//! deliberately not copied.
//!
//! Donor attribution: Copyright the spatial-audio-sim-v2 contributors,
//! Apache-2.0. Adapted for Fightbox's Phase B Gate 0 evidence lane.

mod dsp;
mod extractors;

pub mod corpus;
#[cfg(test)]
mod gate0;

pub use extractors::{
    AnalysisError, COHERENCE_BANDS_HZ, ExtractorMetrics, Pcm, SchroederDecay, analyze,
    schroeder_decay,
};
