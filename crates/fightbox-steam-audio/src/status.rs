//! Honest backend/runtime capability status facade.
//!
//! Relocated from `fightbox-runtime` on 2026-07-29 when the dependency arrow
//! flipped (backend now depends on the runtime spine for the engine-owned
//! snapshot primitive and seam traits). This is intentionally a status
//! surface, not a simulation implementation: it does not infer successful S0
//! or S3 execution from configuration, probe counts, or non-null handles.

use crate::{
    BackendAvailability, STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, backend_availability,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateStatus {
    NotRun,
}

impl GateStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        "not_run"
    }
}

/// Capability whose use needs an SDK-backed world or a completed bake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityStatus {
    Available,
    Unavailable { reason: &'static str },
    NotEstablished { reason: &'static str },
}

/// Honest status surface suitable for hosts and diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub backend: BackendAvailability,
    pub direct: CapabilityStatus,
    pub reflections: CapabilityStatus,
    pub baked_pathing: CapabilityStatus,
    pub s0: GateStatus,
    pub s3: GateStatus,
}

#[must_use]
pub const fn runtime_status() -> RuntimeStatus {
    let backend = backend_availability();
    let direct = match backend {
        BackendAvailability::Available { .. } => CapabilityStatus::NotEstablished {
            reason: "no world or simulation has run",
        },
        BackendAvailability::Unavailable(metadata) => CapabilityStatus::Unavailable {
            reason: metadata.reason,
        },
    };
    RuntimeStatus {
        backend,
        direct,
        reflections: CapabilityStatus::NotEstablished {
            reason: "no reflection simulation has run",
        },
        baked_pathing: CapabilityStatus::NotEstablished {
            reason: "no path bake, serialization, or fresh-process reload has run",
        },
        s0: GateStatus::NotRun,
        s3: GateStatus::NotRun,
    }
}

/// Pinned backend provenance independent of whether this binary was SDK-linked.
#[must_use]
pub const fn steam_audio_provenance() -> (&'static str, &'static str) {
    (STEAM_AUDIO_VERSION, STEAM_AUDIO_UPSTREAM_COMMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_never_claims_an_unrun_gate() {
        let status = runtime_status();
        assert_eq!(status.s0, GateStatus::NotRun);
        assert_eq!(status.s3, GateStatus::NotRun);
        assert_eq!(GateStatus::NotRun.as_str(), "not_run");
        assert!(!matches!(status.baked_pathing, CapabilityStatus::Available));
        assert!(!matches!(status.reflections, CapabilityStatus::Available));
    }

    #[test]
    fn provenance_is_pinned() {
        assert_eq!(steam_audio_provenance(), ("4.8.1", "0da1825"));
    }
}
