//! Lock-free control publication for the final workbench monitor route.

use crate::{MAX_ACTIVE_SOURCES, SnapshotPublication, SnapshotReader, SnapshotWriter};

/// Fixed pad applied to decoded asset PCM on the raw comparison route.
///
/// The workbench's default +30 dB monitor gain therefore reproduces the
/// descriptor-normalized asset level. Per-source audition trim is already
/// present in decoded PCM; scene drive, propagation, and proximity gain are
/// deliberately absent from this diagnostic route.
pub const RAW_MONITOR_PAD_DB: f32 = -30.0;
pub const RAW_MONITOR_PAD_GAIN: f32 = 0.031_622_775;

/// The signal presented to the final monitor-gain and limiter stages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MonitorRoute {
    /// Normal spatial render output from the backend.
    #[default]
    Spatial,
    /// Dual-mono decoded PCM from one source, with the spatial bus suppressed.
    RawSource { source_index: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorRouteError {
    InvalidSourceIndex,
}

/// Control-thread endpoint for changing the monitor route.
pub struct MonitorRouteController {
    writer: SnapshotWriter<MonitorRoute>,
}

impl MonitorRouteController {
    pub fn select_spatial(&mut self) {
        self.writer.publish(MonitorRoute::Spatial);
    }

    pub fn select_raw_source(&mut self, source_index: usize) -> Result<(), MonitorRouteError> {
        if source_index >= MAX_ACTIVE_SOURCES {
            return Err(MonitorRouteError::InvalidSourceIndex);
        }
        self.writer
            .publish(MonitorRoute::RawSource { source_index });
        Ok(())
    }
}

/// Callback-side endpoint. Reading is bounded and wait-free.
pub struct MonitorRouteReader {
    reader: SnapshotReader<MonitorRoute>,
}

impl MonitorRouteReader {
    pub(crate) fn read(&mut self) -> MonitorRoute {
        self.reader.read()
    }
}

/// Factory for the single-writer, single-reader monitor-route channel.
pub struct MonitorRoutePublication;

impl MonitorRoutePublication {
    #[must_use]
    pub fn new() -> (MonitorRouteController, MonitorRouteReader) {
        let (writer, reader) = SnapshotPublication::new(MonitorRoute::Spatial);
        (
            MonitorRouteController { writer },
            MonitorRouteReader { reader },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_switches_routes_and_rejects_invalid_sources() {
        let (mut controller, mut reader) = MonitorRoutePublication::new();
        assert_eq!(reader.read(), MonitorRoute::Spatial);

        controller.select_raw_source(3).unwrap();
        assert_eq!(reader.read(), MonitorRoute::RawSource { source_index: 3 });

        assert_eq!(
            controller.select_raw_source(MAX_ACTIVE_SOURCES),
            Err(MonitorRouteError::InvalidSourceIndex)
        );
        assert_eq!(reader.read(), MonitorRoute::RawSource { source_index: 3 });

        controller.select_spatial();
        assert_eq!(reader.read(), MonitorRoute::Spatial);
    }
}
