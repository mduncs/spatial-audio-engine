//! Control-thread ownership and admission for streamed acoustic cells.
//!
//! The manager deliberately knows nothing about a prepared world's concrete
//! type. Loading, decompression, and backend preparation remain caller work;
//! this module only controls how many worlds may coexist and when authority
//! may move between them.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

pub const MIB: u64 = 1024 * 1024;
pub const DEFAULT_ADVISORY_RESERVE_BYTES: u64 = 512 * MIB;
pub const DEFAULT_ACTIVE_PREPARED_TARGET_BYTES: u64 = 512 * MIB;
pub const DEFAULT_PREPARATION_PEAK_BYTES: u64 = 640 * MIB;
pub const DEFAULT_RAW_CELL_HARD_LIMIT_BYTES: u64 = 64 * MIB;

/// Stable package identities. The runtime compares and reports these strings,
/// but never parses their city-specific format.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CellIdentity {
    pub city: String,
    pub cell: String,
}

impl CellIdentity {
    #[must_use]
    pub fn new(city: impl Into<String>, cell: impl Into<String>) -> Self {
        Self {
            city: city.into(),
            cell: cell.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellStreamingLimits {
    pub minimum_advisory_reserve_bytes: u64,
    pub active_prepared_target_bytes: u64,
    pub preparation_peak_limit_bytes: u64,
    pub raw_cell_hard_limit_bytes: u64,
}

impl Default for CellStreamingLimits {
    fn default() -> Self {
        Self {
            minimum_advisory_reserve_bytes: DEFAULT_ADVISORY_RESERVE_BYTES,
            active_prepared_target_bytes: DEFAULT_ACTIVE_PREPARED_TARGET_BYTES,
            preparation_peak_limit_bytes: DEFAULT_PREPARATION_PEAK_BYTES,
            raw_cell_hard_limit_bytes: DEFAULT_RAW_CELL_HARD_LIMIT_BYTES,
        }
    }
}

/// A sample supplied immediately before admission. Passing it at the admission
/// boundary makes the host's responsibility to resample explicit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FreshMemorySample {
    pub advisory_reserve_bytes: u64,
    pub process_resident_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellPrepareEstimate {
    pub raw_cell_bytes: u64,
    pub prepared_resident_bytes: u64,
    /// Temporary bytes above active plus the final prepared world.
    pub preparation_scratch_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteCellCandidate {
    pub identity: CellIdentity,
    /// Signed displacement along the authored route from the listener.
    pub route_offset_mm: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteDirection {
    Forward,
    Reverse,
    Stationary,
}

/// Chooses the nearest cell in the direction of travel. Exact-distance ties use
/// stable identity order, so package enumeration order cannot change prefetch.
#[must_use]
pub fn choose_route_candidate(
    active: &CellIdentity,
    direction: RouteDirection,
    candidates: &[RouteCellCandidate],
) -> Option<CellIdentity> {
    candidates
        .iter()
        .filter(|candidate| candidate.identity != *active)
        .filter(|candidate| match direction {
            RouteDirection::Forward => candidate.route_offset_mm >= 0,
            RouteDirection::Reverse => candidate.route_offset_mm <= 0,
            RouteDirection::Stationary => true,
        })
        .min_by_key(|candidate| {
            (
                candidate.route_offset_mm.unsigned_abs(),
                &candidate.identity,
            )
        })
        .map(|candidate| candidate.identity.clone())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PrepareTicket(u64);

impl PrepareTicket {
    #[must_use]
    pub const fn serial(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareJob {
    pub ticket: PrepareTicket,
    pub identity: Arc<CellIdentity>,
    pub estimate: CellPrepareEstimate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareRefusalReason {
    TailRetiring,
    PreparationInFlight {
        identity: Arc<CellIdentity>,
    },
    AlreadyActive,
    RawCellTooLarge {
        requested: u64,
        limit: u64,
    },
    AdvisoryReserveTooLow {
        sampled: u64,
        required: u64,
    },
    ActivePreparedTargetExceeded {
        projected: u64,
        limit: u64,
    },
    PreparationPeakExceeded {
        projected: u64,
        limit: u64,
    },
    CompletedPayloadTooLarge {
        projected: u64,
        limit: u64,
    },
    RouteChangedWhilePreparing {
        prepared: Arc<CellIdentity>,
        requested: Arc<CellIdentity>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareCancellationReason {
    RouteChanged {
        cancelled: Arc<CellIdentity>,
        replacement: Arc<CellIdentity>,
    },
    Explicit {
        cancelled: Arc<CellIdentity>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareFailureReason {
    Loader(String),
    CancelledAfterStart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareAdmission {
    Queued(PrepareTicket),
    AlreadyPending(PrepareTicket),
    ReplacedUnstarted {
        cancelled: CellIdentity,
        queued: PrepareTicket,
    },
    Refused(PrepareRefusalReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompletePreparation {
    Prepared,
    Rejected(PrepareRefusalReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CellStreamingError {
    UnknownTicket(PrepareTicket),
    PreparationNotQueued,
    PreparationNotInFlight,
    PreparationNotReady,
    IdentityMismatch,
    TailAlreadyRetiring,
}

impl fmt::Display for CellStreamingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cell streaming state error: {self:?}")
    }
}

impl std::error::Error for CellStreamingError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NeighborStateTelemetry {
    Queued,
    Preparing,
    Prepared,
}

/// The identity aliases the neighbor job's `Arc` allocation instead of owning
/// a deep copy, so telemetry construction performs no string allocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NeighborTelemetry {
    pub identity: Arc<CellIdentity>,
    pub state: NeighborStateTelemetry,
    pub estimated_resident_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellStreamTelemetry {
    pub active: Arc<CellIdentity>,
    pub last_requested: Option<Arc<CellIdentity>>,
    pub neighbor: Option<NeighborTelemetry>,
    pub tail_retiring: Option<Arc<CellIdentity>>,
    pub resident_world_bytes: u64,
    pub observed_peak_process_bytes: u64,
    pub last_projected_prepare_peak_bytes: Option<u64>,
    pub last_prepare_latency: Option<Duration>,
    pub last_refusal: Option<PrepareRefusalReason>,
    pub last_cancellation: Option<PrepareCancellationReason>,
    pub last_failure: Option<PrepareFailureReason>,
    pub coarse_macro_fallback: bool,
}

struct ResidentCell<P> {
    identity: Arc<CellIdentity>,
    payload: P,
    resident_bytes: u64,
}

enum Neighbor<P> {
    Queued {
        ticket: PrepareTicket,
        identity: Arc<CellIdentity>,
        estimate: CellPrepareEstimate,
        sampled_process_resident_bytes: u64,
    },
    Preparing {
        ticket: PrepareTicket,
        identity: Arc<CellIdentity>,
        estimate: CellPrepareEstimate,
        sampled_process_resident_bytes: u64,
        started_at: Duration,
    },
    Prepared {
        ticket: PrepareTicket,
        cell: ResidentCell<P>,
        latency: Duration,
    },
}

impl<P> Neighbor<P> {
    fn ticket(&self) -> PrepareTicket {
        match self {
            Self::Queued { ticket, .. }
            | Self::Preparing { ticket, .. }
            | Self::Prepared { ticket, .. } => *ticket,
        }
    }

    fn identity(&self) -> &CellIdentity {
        match self {
            Self::Queued { identity, .. } | Self::Preparing { identity, .. } => identity,
            Self::Prepared { cell, .. } => &cell.identity,
        }
    }

    fn identity_shared(&self) -> &Arc<CellIdentity> {
        match self {
            Self::Queued { identity, .. } | Self::Preparing { identity, .. } => identity,
            Self::Prepared { cell, .. } => &cell.identity,
        }
    }
}

/// A control-thread state machine. It performs no I/O and has no callback API.
pub struct CellStreamManager<P> {
    limits: CellStreamingLimits,
    active: ResidentCell<P>,
    neighbor: Option<Neighbor<P>>,
    retiring: Option<ResidentCell<P>>,
    next_ticket: u64,
    observed_peak_process_bytes: u64,
    last_projected_prepare_peak_bytes: Option<u64>,
    last_prepare_latency: Option<Duration>,
    last_requested: Option<Arc<CellIdentity>>,
    last_refusal: Option<PrepareRefusalReason>,
    last_cancellation: Option<PrepareCancellationReason>,
    last_failure: Option<PrepareFailureReason>,
    coarse_macro_fallback: bool,
}

impl<P> CellStreamManager<P> {
    #[must_use]
    pub fn new(
        active_identity: CellIdentity,
        active_payload: P,
        active_resident_bytes: u64,
        limits: CellStreamingLimits,
    ) -> Self {
        Self {
            limits,
            active: ResidentCell {
                identity: Arc::new(active_identity),
                payload: active_payload,
                resident_bytes: active_resident_bytes,
            },
            neighbor: None,
            retiring: None,
            next_ticket: 1,
            observed_peak_process_bytes: active_resident_bytes,
            last_projected_prepare_peak_bytes: None,
            last_prepare_latency: None,
            last_requested: None,
            last_refusal: None,
            last_cancellation: None,
            last_failure: None,
            coarse_macro_fallback: false,
        }
    }

    #[must_use]
    pub fn active_identity(&self) -> &CellIdentity {
        &self.active.identity
    }

    #[must_use]
    pub fn active_payload(&self) -> &P {
        &self.active.payload
    }

    /// Admits a candidate after a fresh process-memory sample. If route intent
    /// changes while the prior candidate is still queued, that unstarted job is
    /// cancelled deterministically and replaced.
    pub fn request_prepare(
        &mut self,
        identity: CellIdentity,
        estimate: CellPrepareEstimate,
        sample: FreshMemorySample,
    ) -> PrepareAdmission {
        let identity = Arc::new(identity);
        self.last_requested = Some(Arc::clone(&identity));
        self.observed_peak_process_bytes = self
            .observed_peak_process_bytes
            .max(sample.process_resident_bytes);

        if *identity == *self.active.identity {
            return self.refuse(PrepareRefusalReason::AlreadyActive);
        }
        if self.retiring.is_some() {
            return self.refuse(PrepareRefusalReason::TailRetiring);
        }
        if let Some(neighbor) = &self.neighbor {
            if neighbor.identity() == identity.as_ref() {
                return PrepareAdmission::AlreadyPending(neighbor.ticket());
            }
            if !matches!(neighbor, Neighbor::Queued { .. }) {
                return self.refuse(PrepareRefusalReason::PreparationInFlight {
                    identity: Arc::clone(neighbor.identity_shared()),
                });
            }
        }
        if let Some(reason) = self.admission_refusal(estimate, &sample) {
            return self.refuse(reason);
        }

        let sampled_process_resident_bytes = sample.process_resident_bytes;
        self.last_projected_prepare_peak_bytes = Some(
            sampled_process_resident_bytes
                .saturating_add(estimate.prepared_resident_bytes)
                .saturating_add(estimate.preparation_scratch_bytes),
        );

        let replaced = self.neighbor.take().map(|neighbor| {
            let cancelled = Arc::clone(neighbor.identity_shared());
            self.last_cancellation = Some(PrepareCancellationReason::RouteChanged {
                cancelled: Arc::clone(&cancelled),
                replacement: Arc::clone(&identity),
            });
            (*cancelled).clone()
        });
        let ticket = PrepareTicket(self.next_ticket);
        self.next_ticket = self.next_ticket.wrapping_add(1).max(1);
        self.neighbor = Some(Neighbor::Queued {
            ticket,
            identity,
            estimate,
            sampled_process_resident_bytes,
        });
        self.last_refusal = None;
        self.coarse_macro_fallback = false;
        replaced.map_or(PrepareAdmission::Queued(ticket), |cancelled| {
            PrepareAdmission::ReplacedUnstarted {
                cancelled,
                queued: ticket,
            }
        })
    }

    fn admission_refusal(
        &self,
        estimate: CellPrepareEstimate,
        sample: &FreshMemorySample,
    ) -> Option<PrepareRefusalReason> {
        if estimate.raw_cell_bytes > self.limits.raw_cell_hard_limit_bytes {
            return Some(PrepareRefusalReason::RawCellTooLarge {
                requested: estimate.raw_cell_bytes,
                limit: self.limits.raw_cell_hard_limit_bytes,
            });
        }
        if sample.advisory_reserve_bytes < self.limits.minimum_advisory_reserve_bytes {
            return Some(PrepareRefusalReason::AdvisoryReserveTooLow {
                sampled: sample.advisory_reserve_bytes,
                required: self.limits.minimum_advisory_reserve_bytes,
            });
        }
        let active_prepared = sample
            .process_resident_bytes
            .saturating_add(estimate.prepared_resident_bytes);
        if active_prepared > self.limits.active_prepared_target_bytes {
            return Some(PrepareRefusalReason::ActivePreparedTargetExceeded {
                projected: active_prepared,
                limit: self.limits.active_prepared_target_bytes,
            });
        }
        let preparation_peak = active_prepared.saturating_add(estimate.preparation_scratch_bytes);
        if preparation_peak > self.limits.preparation_peak_limit_bytes {
            return Some(PrepareRefusalReason::PreparationPeakExceeded {
                projected: preparation_peak,
                limit: self.limits.preparation_peak_limit_bytes,
            });
        }
        None
    }

    fn refuse(&mut self, reason: PrepareRefusalReason) -> PrepareAdmission {
        self.last_refusal = Some(reason.clone());
        self.coarse_macro_fallback = true;
        PrepareAdmission::Refused(reason)
    }

    /// Marks the queued job as started and returns all data needed by an
    /// external loader. The returned job is safe to move to an I/O worker.
    pub fn start_prepare(
        &mut self,
        ticket: PrepareTicket,
        now: Duration,
    ) -> Result<PrepareJob, CellStreamingError> {
        let Some(neighbor) = self.neighbor.take() else {
            return Err(CellStreamingError::UnknownTicket(ticket));
        };
        if neighbor.ticket() != ticket {
            self.neighbor = Some(neighbor);
            return Err(CellStreamingError::UnknownTicket(ticket));
        }
        let Neighbor::Queued {
            ticket,
            identity,
            estimate,
            sampled_process_resident_bytes,
        } = neighbor
        else {
            self.neighbor = Some(neighbor);
            return Err(CellStreamingError::PreparationNotQueued);
        };
        let job = PrepareJob {
            ticket,
            identity: Arc::clone(&identity),
            estimate,
        };
        self.neighbor = Some(Neighbor::Preparing {
            ticket,
            identity,
            estimate,
            sampled_process_resident_bytes,
            started_at: now,
        });
        Ok(job)
    }

    /// Publishes an opaque prepared world. Oversized actual payloads are
    /// rejected before becoming resident, preserving active authority.
    pub fn complete_prepare(
        &mut self,
        ticket: PrepareTicket,
        payload: P,
        actual_resident_bytes: u64,
        now: Duration,
    ) -> Result<CompletePreparation, CellStreamingError> {
        let Some(neighbor) = self.neighbor.take() else {
            return Err(CellStreamingError::UnknownTicket(ticket));
        };
        if neighbor.ticket() != ticket {
            self.neighbor = Some(neighbor);
            return Err(CellStreamingError::UnknownTicket(ticket));
        }
        let Neighbor::Preparing {
            ticket,
            identity,
            sampled_process_resident_bytes,
            started_at,
            ..
        } = neighbor
        else {
            self.neighbor = Some(neighbor);
            return Err(CellStreamingError::PreparationNotInFlight);
        };
        let latency = now.saturating_sub(started_at);
        self.last_prepare_latency = Some(latency);
        if self.last_requested.as_deref() != Some(identity.as_ref()) {
            let reason = PrepareRefusalReason::RouteChangedWhilePreparing {
                prepared: Arc::clone(&identity),
                requested: Arc::clone(
                    self.last_requested
                        .as_ref()
                        .expect("a preparing neighbor always has a last request"),
                ),
            };
            self.last_refusal = Some(reason.clone());
            self.coarse_macro_fallback = true;
            return Ok(CompletePreparation::Rejected(reason));
        }
        let projected = sampled_process_resident_bytes.saturating_add(actual_resident_bytes);
        if projected > self.limits.active_prepared_target_bytes {
            let reason = PrepareRefusalReason::CompletedPayloadTooLarge {
                projected,
                limit: self.limits.active_prepared_target_bytes,
            };
            self.last_refusal = Some(reason.clone());
            self.coarse_macro_fallback = true;
            return Ok(CompletePreparation::Rejected(reason));
        }
        self.neighbor = Some(Neighbor::Prepared {
            ticket,
            cell: ResidentCell {
                identity,
                payload,
                resident_bytes: actual_resident_bytes,
            },
            latency,
        });
        self.coarse_macro_fallback = false;
        Ok(CompletePreparation::Prepared)
    }

    pub fn fail_prepare(
        &mut self,
        ticket: PrepareTicket,
        reason: PrepareFailureReason,
    ) -> Result<(), CellStreamingError> {
        let Some(neighbor) = &self.neighbor else {
            return Err(CellStreamingError::UnknownTicket(ticket));
        };
        if neighbor.ticket() != ticket {
            return Err(CellStreamingError::UnknownTicket(ticket));
        }
        self.neighbor = None;
        self.last_failure = Some(reason);
        self.coarse_macro_fallback = true;
        Ok(())
    }

    pub fn cancel_unstarted(&mut self, ticket: PrepareTicket) -> Result<(), CellStreamingError> {
        let Some(neighbor) = self.neighbor.take() else {
            return Err(CellStreamingError::UnknownTicket(ticket));
        };
        if neighbor.ticket() != ticket {
            self.neighbor = Some(neighbor);
            return Err(CellStreamingError::UnknownTicket(ticket));
        }
        if !matches!(neighbor, Neighbor::Queued { .. }) {
            self.neighbor = Some(neighbor);
            return Err(CellStreamingError::PreparationNotQueued);
        }
        self.last_cancellation = Some(PrepareCancellationReason::Explicit {
            cancelled: Arc::clone(neighbor.identity_shared()),
        });
        self.coarse_macro_fallback = true;
        Ok(())
    }

    /// Transfers new-event authority to a prepared cell. The former active
    /// payload stays owned until its admitted acoustic tails explicitly finish.
    pub fn adopt_prepared(&mut self, ticket: PrepareTicket) -> Result<(), CellStreamingError> {
        if self.retiring.is_some() {
            return Err(CellStreamingError::TailAlreadyRetiring);
        }
        let Some(neighbor) = self.neighbor.take() else {
            return Err(CellStreamingError::UnknownTicket(ticket));
        };
        if neighbor.ticket() != ticket {
            self.neighbor = Some(neighbor);
            return Err(CellStreamingError::UnknownTicket(ticket));
        }
        let Neighbor::Prepared { cell, latency, .. } = neighbor else {
            self.neighbor = Some(neighbor);
            return Err(CellStreamingError::PreparationNotReady);
        };
        let old = std::mem::replace(&mut self.active, cell);
        self.retiring = Some(old);
        self.last_prepare_latency = Some(latency);
        self.coarse_macro_fallback =
            self.last_requested.as_deref() != Some(self.active.identity.as_ref());
        Ok(())
    }

    /// Drops the old world's tail authority and reopens neighbor preparation.
    pub fn finish_tail_retirement(&mut self) -> Option<P> {
        self.retiring.take().map(|cell| cell.payload)
    }

    #[must_use]
    pub fn telemetry(&self) -> CellStreamTelemetry {
        let neighbor = self.neighbor.as_ref().map(|neighbor| {
            let state = match neighbor {
                Neighbor::Queued { .. } => NeighborStateTelemetry::Queued,
                Neighbor::Preparing { .. } => NeighborStateTelemetry::Preparing,
                Neighbor::Prepared { .. } => NeighborStateTelemetry::Prepared,
            };
            let estimated_resident_bytes = match neighbor {
                Neighbor::Queued { estimate, .. } | Neighbor::Preparing { estimate, .. } => {
                    estimate.prepared_resident_bytes
                }
                Neighbor::Prepared { cell, .. } => cell.resident_bytes,
            };
            NeighborTelemetry {
                identity: Arc::clone(neighbor.identity_shared()),
                state,
                estimated_resident_bytes,
            }
        });
        let neighbor_bytes = match &self.neighbor {
            Some(Neighbor::Prepared { cell, .. }) => cell.resident_bytes,
            _ => 0,
        };
        let retiring_bytes = self.retiring.as_ref().map_or(0, |cell| cell.resident_bytes);
        CellStreamTelemetry {
            active: Arc::clone(&self.active.identity),
            last_requested: self.last_requested.clone(),
            neighbor,
            tail_retiring: self
                .retiring
                .as_ref()
                .map(|cell| Arc::clone(&cell.identity)),
            resident_world_bytes: self
                .active
                .resident_bytes
                .saturating_add(neighbor_bytes)
                .saturating_add(retiring_bytes),
            observed_peak_process_bytes: self.observed_peak_process_bytes,
            last_projected_prepare_peak_bytes: self.last_projected_prepare_peak_bytes,
            last_prepare_latency: self.last_prepare_latency,
            last_refusal: self.last_refusal.clone(),
            last_cancellation: self.last_cancellation.clone(),
            last_failure: self.last_failure.clone(),
            coarse_macro_fallback: self.coarse_macro_fallback,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(cell: &str) -> CellIdentity {
        CellIdentity::new("chi", cell)
    }

    fn estimate(prepared_mib: u64) -> CellPrepareEstimate {
        CellPrepareEstimate {
            raw_cell_bytes: 40 * MIB,
            prepared_resident_bytes: prepared_mib * MIB,
            preparation_scratch_bytes: 80 * MIB,
        }
    }

    fn ample_memory() -> FreshMemorySample {
        FreshMemorySample {
            advisory_reserve_bytes: 900 * MIB,
            process_resident_bytes: 210 * MIB,
        }
    }

    #[test]
    fn route_choice_is_directional_and_stably_tied() {
        let active = id("e0:n0");
        let candidates = [
            RouteCellCandidate {
                identity: id("e2:n0"),
                route_offset_mm: 485_000,
            },
            RouteCellCandidate {
                identity: id("e1:n0"),
                route_offset_mm: 485_000,
            },
            RouteCellCandidate {
                identity: id("e-1:n0"),
                route_offset_mm: -200_000,
            },
        ];
        assert_eq!(
            choose_route_candidate(&active, RouteDirection::Forward, &candidates),
            Some(id("e1:n0"))
        );
        assert_eq!(
            choose_route_candidate(&active, RouteDirection::Reverse, &candidates),
            Some(id("e-1:n0"))
        );
    }

    #[test]
    fn multiway_prediction_miss_drops_stale_completion_before_replacement() {
        let mut manager = CellStreamManager::new(id("a"), "world-a", 180 * MIB, Default::default());
        let first = match manager.request_prepare(id("b"), estimate(190), ample_memory()) {
            PrepareAdmission::Queued(ticket) => ticket,
            other => panic!("unexpected admission {other:?}"),
        };
        let second = match manager.request_prepare(id("c"), estimate(190), ample_memory()) {
            PrepareAdmission::ReplacedUnstarted { cancelled, queued } => {
                assert_eq!(cancelled, id("b"));
                queued
            }
            other => panic!("unexpected replacement {other:?}"),
        };
        assert!(matches!(
            manager.start_prepare(second, Duration::from_secs(4)),
            Ok(PrepareJob { identity, .. }) if *identity == id("c")
        ));
        assert!(matches!(
            manager.request_prepare(id("d"), estimate(190), ample_memory()),
            PrepareAdmission::Refused(PrepareRefusalReason::PreparationInFlight { identity })
                if *identity == id("c")
        ));
        assert_eq!(
            manager
                .complete_prepare(second, "world-c", 185 * MIB, Duration::from_secs(5))
                .unwrap(),
            CompletePreparation::Rejected(PrepareRefusalReason::RouteChangedWhilePreparing {
                prepared: Arc::new(id("c")),
                requested: Arc::new(id("d")),
            })
        );
        let telemetry = manager.telemetry();
        assert!(telemetry.coarse_macro_fallback);
        assert!(telemetry.neighbor.is_none());
        assert!(matches!(
            manager.request_prepare(id("d"), estimate(190), ample_memory()),
            PrepareAdmission::Queued(_)
        ));
        assert_eq!(first.serial(), 1);
    }

    #[test]
    fn adoption_retains_old_tail_and_blocks_a_third_world() {
        let mut manager = CellStreamManager::new(id("a"), "world-a", 180 * MIB, Default::default());
        let ticket = match manager.request_prepare(id("b"), estimate(190), ample_memory()) {
            PrepareAdmission::Queued(ticket) => ticket,
            other => panic!("unexpected admission {other:?}"),
        };
        manager
            .start_prepare(ticket, Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            manager
                .complete_prepare(ticket, "world-b", 185 * MIB, Duration::from_millis(850))
                .unwrap(),
            CompletePreparation::Prepared
        );
        manager.adopt_prepared(ticket).unwrap();
        assert_eq!(manager.active_payload(), &"world-b");
        let telemetry = manager.telemetry();
        assert_eq!(telemetry.tail_retiring, Some(Arc::new(id("a"))));
        assert_eq!(
            telemetry.last_prepare_latency,
            Some(Duration::from_millis(750))
        );
        assert_eq!(telemetry.resident_world_bytes, 365 * MIB);
        assert!(matches!(
            manager.request_prepare(id("c"), estimate(180), ample_memory()),
            PrepareAdmission::Refused(PrepareRefusalReason::TailRetiring)
        ));
        assert_eq!(manager.finish_tail_retirement(), Some("world-a"));
        assert!(matches!(
            manager.request_prepare(id("c"), estimate(180), ample_memory()),
            PrepareAdmission::Queued(_)
        ));
    }

    #[test]
    fn memory_refusal_and_loader_failure_preserve_active_authority() {
        let mut manager = CellStreamManager::new(id("a"), "world-a", 300 * MIB, Default::default());
        assert!(matches!(
            manager.request_prepare(
                id("b"),
                CellPrepareEstimate {
                    raw_cell_bytes: 65 * MIB,
                    prepared_resident_bytes: 100 * MIB,
                    preparation_scratch_bytes: 10 * MIB,
                },
                ample_memory()
            ),
            PrepareAdmission::Refused(PrepareRefusalReason::RawCellTooLarge { .. })
        ));
        assert_eq!(manager.active_payload(), &"world-a");
        assert!(manager.telemetry().coarse_macro_fallback);

        assert!(matches!(
            manager.request_prepare(
                id("b"),
                estimate(100),
                FreshMemorySample {
                    advisory_reserve_bytes: 511 * MIB,
                    process_resident_bytes: 300 * MIB,
                }
            ),
            PrepareAdmission::Refused(PrepareRefusalReason::AdvisoryReserveTooLow { .. })
        ));
        assert!(matches!(
            manager.request_prepare(
                id("b"),
                estimate(150),
                FreshMemorySample {
                    advisory_reserve_bytes: 900 * MIB,
                    process_resident_bytes: 400 * MIB,
                }
            ),
            PrepareAdmission::Refused(PrepareRefusalReason::ActivePreparedTargetExceeded { .. })
        ));
        assert!(matches!(
            manager.request_prepare(
                id("b"),
                CellPrepareEstimate {
                    raw_cell_bytes: 40 * MIB,
                    prepared_resident_bytes: 100 * MIB,
                    preparation_scratch_bytes: 150 * MIB,
                },
                FreshMemorySample {
                    advisory_reserve_bytes: 900 * MIB,
                    process_resident_bytes: 400 * MIB,
                }
            ),
            PrepareAdmission::Refused(PrepareRefusalReason::PreparationPeakExceeded { .. })
        ));

        let ticket = match manager.request_prepare(id("b"), estimate(150), ample_memory()) {
            PrepareAdmission::Queued(ticket) => ticket,
            other => panic!("unexpected admission {other:?}"),
        };
        manager.start_prepare(ticket, Duration::ZERO).unwrap();
        manager
            .fail_prepare(ticket, PrepareFailureReason::Loader("bad hash".into()))
            .unwrap();
        assert_eq!(manager.active_identity(), &id("a"));
        assert!(manager.telemetry().coarse_macro_fallback);
    }
}
