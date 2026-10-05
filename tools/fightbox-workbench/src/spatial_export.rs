use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use fightbox_api::ListenerState;
use fightbox_runtime::backend::{
    MAX_ACTIVE_SOURCES, MAX_SPATIAL_ENVIRONMENT_PLANES, MAX_SPATIAL_PRESENTATION_FEEDS,
    SpatialOutputMetadata, SpatialOutputValidity, SpatialProcessBlock, SpatialProgramBlock,
};
use fightbox_runtime::{
    BlockProcessor, ProcessBlock, ProgramProcessBlock, RenderError, RuntimeGraph,
};

// One writer on the paced render thread; read only after that thread has stopped.
pub(crate) struct StemCapture {
    samples: Box<[AtomicU32]>,
    written: AtomicUsize,
    failed: AtomicBool,
}

impl StemCapture {
    pub fn new(frames: usize) -> Arc<Self> {
        Arc::new(Self {
            samples: (0..frames * 9).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
        })
    }

    fn append(&self, samples: &[f32], audible: bool) {
        let start = self.written.load(Ordering::Relaxed);
        let Some(end) = start
            .checked_add(samples.len())
            .filter(|end| *end <= self.samples.len())
        else {
            self.failed.store(true, Ordering::Relaxed);
            return;
        };
        for (out, sample) in self.samples[start..end].iter().zip(samples) {
            out.store(
                if audible { sample.to_bits() } else { 0 },
                Ordering::Relaxed,
            );
        }
        self.written.store(end, Ordering::Release);
    }

    pub fn finish(&self) -> Result<Vec<f32>, String> {
        if self.failed.load(Ordering::Acquire) {
            return Err(
                "AmbiX render failed: invalid spatial block or export buffer overflow".into(),
            );
        }
        if self.written.load(Ordering::Acquire) != self.samples.len() {
            return Err("AmbiX render did not complete the requested audio frames".into());
        }
        Ok(self
            .samples
            .iter()
            .map(|sample| f32::from_bits(sample.load(Ordering::Relaxed)))
            .collect())
    }
}

pub(crate) struct SpatialExportGraph {
    graph: RuntimeGraph,
    listener: ListenerState,
    presentation: Vec<f32>,
    environment: Vec<f32>,
    metadata: SpatialOutputMetadata,
    interleaved: Vec<f32>,
    block_start_frame: u64,
    capture: Arc<StemCapture>,
}

impl SpatialExportGraph {
    pub fn new(graph: RuntimeGraph, listener: ListenerState, capture: Arc<StemCapture>) -> Self {
        let frames = graph.block_size_frames();
        Self {
            graph,
            listener,
            presentation: vec![0.0; MAX_SPATIAL_PRESENTATION_FEEDS * frames],
            environment: vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * frames],
            metadata: SpatialOutputMetadata::default(),
            interleaved: vec![0.0; frames * 9],
            block_start_frame: 0,
            capture,
        }
    }

    pub fn set_listener_state(&mut self, listener: ListenerState) {
        self.listener = listener;
        self.graph.set_listener_state(listener);
    }

    pub fn capture_block(&self, audible: bool) {
        self.capture.append(&self.interleaved, audible);
    }
}

impl BlockProcessor for SpatialExportGraph {
    fn block_size_frames(&self) -> usize {
        self.graph.block_size_frames()
    }

    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
        if block.sources.len() > MAX_ACTIVE_SOURCES {
            return Err(RenderError::TooManySources);
        }
        let sources: [SpatialProgramBlock<'_>; MAX_ACTIVE_SOURCES] = std::array::from_fn(|slot| {
            let source = block.sources.get(slot);
            SpatialProgramBlock {
                source_index: source.map_or(0, |source| source.source_index),
                program_plane_count: 1,
                program_planes: [source.map_or(&[][..], |source| source.decoded_mono), &[]],
            }
        });
        self.process_program_block(ProgramProcessBlock {
            now_ns: block.now_ns,
            sources: &sources[..block.sources.len()],
            output_left: block.output_left,
            output_right: block.output_right,
        })
    }

    fn process_program_block(&mut self, block: ProgramProcessBlock<'_>) -> Result<(), RenderError> {
        // Export never goes to a listening device. The live binaural graph and
        // its original limiter remain untouched; stems have a fail-closed peak check.
        block.output_left.fill(0.0);
        block.output_right.fill(0.0);
        let frames = self.block_size_frames();
        let result = self.graph.process_spatial_block(SpatialProcessBlock {
            now_ns: block.now_ns,
            block_start_frame: self.block_start_frame,
            sources: block.sources,
            presentation_bank: &mut self.presentation,
            environmental_bank: &mut self.environment,
            metadata: &mut self.metadata,
        });
        if result.is_err() || self.metadata.validity != SpatialOutputValidity::Valid {
            self.capture.failed.store(true, Ordering::Relaxed);
            return Err(RenderError::InvalidPropagation);
        }
        if crate::ambix::encode_block(
            &self.presentation,
            &self.environment,
            &self.metadata,
            self.listener.pose,
            frames,
            &mut self.interleaved,
        )
        .is_err()
        {
            self.capture.failed.store(true, Ordering::Relaxed);
            return Err(RenderError::InvalidPropagation);
        }
        self.block_start_frame += frames as u64;
        Ok(())
    }

    fn fault_counters(&self) -> fightbox_runtime::FaultCounters {
        self.graph.fault_counters()
    }

    fn safety_telemetry(&self) -> fightbox_runtime::SafetyTelemetry {
        self.graph.safety_telemetry()
    }
}
