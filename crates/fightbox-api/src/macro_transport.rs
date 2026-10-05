//! Vendor-neutral declarations for city-scale source and event transport.

use crate::EnuVector3;

/// Stable logical identity of a remote emitter or one-shot event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MacroEventId(pub u64);

/// The four predeclared transient render roles retained by V1 worlds.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventRole {
    #[default]
    CinematicImpulse = 0,
    StandardImpulse = 1,
    BallisticCrack = 2,
    BallisticBlast = 3,
}

impl EventRole {
    pub const COUNT: usize = 4;
    pub const ALL: [Self; Self::COUNT] = [
        Self::CinematicImpulse,
        Self::StandardImpulse,
        Self::BallisticCrack,
        Self::BallisticBlast,
    ];

    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// V1 propagation features this retained role may request after its macro
    /// segment reaches the listener's active acoustic cell.
    ///
    /// Ballistic cracks are deliberately direct-only. A shock front cannot
    /// acquire statistical ground color, a reflection tail, or an authored
    /// echo merely because it shares a family with the muzzle blast.
    #[must_use]
    pub const fn local_propagation_eligibility(self) -> EventPropagationEligibility {
        match self {
            Self::CinematicImpulse | Self::StandardImpulse | Self::BallisticBlast => {
                EventPropagationEligibility {
                    detailed_direct: true,
                    statistical_ground: false,
                    baked_reflections: true,
                    authored_echo: true,
                    shared_diffuse: true,
                }
            }
            Self::BallisticCrack => EventPropagationEligibility {
                detailed_direct: true,
                statistical_ground: false,
                baked_reflections: false,
                authored_echo: false,
                shared_diffuse: false,
            },
        }
    }
}

/// Explicit feature admission for one macro-event role's final local leg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventPropagationEligibility {
    pub detailed_direct: bool,
    pub statistical_ground: bool,
    pub baked_reflections: bool,
    pub authored_echo: bool,
    pub shared_diffuse: bool,
}

/// Whether a source program can survive dormant city-scale transport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MacroAssetTransport {
    #[default]
    Seekable,
    PreGenerated,
    DeterministicGenerator,
    NonSeekableLive,
}

impl MacroAssetTransport {
    #[must_use]
    pub const fn is_supported(self) -> bool {
        !matches!(self, Self::NonSeekableLive)
    }
}

/// Remote source declaration used to derive a listener-relative ingress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacroEmitter {
    pub id: MacroEventId,
    pub position_enu: EnuVector3,
    pub program_started_at_s: f64,
    pub asset_transport: MacroAssetTransport,
    /// True when the recording already contains its own fly-by or Doppler cue.
    pub recording_carries_motion: bool,
}

/// Listener state needed by the macro planner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacroListener {
    pub position_enu: EnuVector3,
    pub session_time_s: f64,
}

/// Local-world horizon used to partition one physical propagation path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MacroTransportConfig {
    pub local_horizon_m: f32,
}

impl Default for MacroTransportConfig {
    fn default() -> Self {
        Self {
            local_horizon_m: 600.0,
        }
    }
}
