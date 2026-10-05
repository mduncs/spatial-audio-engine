use std::any::Any;
use std::cell::UnsafeCell;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::app_tap::{AudioApp, Event, TapHelper, TapStatus};
use fightbox_runtime::live::LiveOutput;
use fightbox_runtime::live_input::{
    AdaptiveInput, LiveCapture, LiveInputTelemetry, LiveInputTelemetryReader, StereoProducer,
    feedback_guard, resolved_output_device_name, stereo_ring,
};

pub struct LiveAudio {
    output: LiveOutput,
    _lifetimes: Vec<Box<dyn Any>>,
    telemetry: Vec<(usize, LiveInputTelemetryReader)>,
    controls: Vec<(usize, InputControl)>,
    apps: Vec<AudioApp>,
    app_list: Option<Receiver<Result<Vec<AudioApp>, String>>>,
    app_list_error: Option<String>,
}

impl LiveAudio {
    pub fn new(
        output: LiveOutput,
        lifetimes: Vec<Box<dyn Any>>,
        telemetry: Vec<(usize, LiveInputTelemetryReader)>,
        controls: Vec<(usize, InputControl)>,
    ) -> Self {
        Self {
            output,
            _lifetimes: lifetimes,
            telemetry,
            app_list: (!controls.is_empty()).then(crate::app_tap::enumerate),
            controls,
            apps: Vec::new(),
            app_list_error: None,
        }
    }

    pub fn input_telemetry(&self) -> impl Iterator<Item = (usize, LiveInputTelemetry)> + '_ {
        self.telemetry
            .iter()
            .map(|(index, reader)| (*index, reader.snapshot()))
            .chain(self.controls.iter().filter_map(|(index, control)| {
                control
                    .telemetry
                    .as_ref()
                    .map(|reader| (*index, reader.snapshot()))
            }))
    }

    pub fn input_telemetry_json(&self) -> serde_json::Value {
        self.input_telemetry()
            .map(|(index, input)| {
                serde_json::json!({
                    "source_index": index, "underruns": input.underruns,
                    "overruns": input.overruns, "fill_frames": input.fill_frames,
                    "fill_ms": input.fill_ms, "ratio": input.ratio,
                    "stream_errors": input.stream_errors,
                })
            })
            .collect::<Vec<_>>()
            .into()
    }

    pub fn poll_inputs(&mut self) {
        if let Some(result) = self
            .app_list
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            self.app_list = None;
            match result {
                Ok(apps) => {
                    self.apps = apps;
                    self.app_list_error = None;
                }
                Err(error) => self.app_list_error = Some(error),
            }
        }
        for (_, control) in &mut self.controls {
            control.poll();
        }
    }

    pub fn start_input(&mut self, index: usize) -> Result<(), String> {
        if let Some((_, control)) = self.controls.iter_mut().find(|(slot, _)| *slot == index) {
            control.start()?;
        }
        Ok(())
    }

    pub fn stop_inputs(&mut self) {
        for (_, control) in &mut self.controls {
            control.stop();
        }
    }

    pub fn select_song(&mut self, index: usize) {
        if let Some((_, control)) = self.controls.iter_mut().find(|(slot, _)| *slot == index) {
            control.stop();
            control.selection = InputSelection::File;
            control.message = "Song file · ready · press Play".into();
        }
    }

    /// Shows a song decoded at startup as the speaker's current choice.
    pub fn show_song(&mut self, index: usize) {
        if let Some((_, control)) = self.controls.iter_mut().find(|(slot, _)| *slot == index)
            && control.selection != InputSelection::File
            && control.helper.is_none()
            && control.capture.is_none()
        {
            control.selection = InputSelection::File;
            control.message = "Song · press Play".into();
        }
    }

    pub fn stop_input(&mut self, index: usize) {
        if let Some((_, control)) = self.controls.iter_mut().find(|(slot, _)| *slot == index) {
            control.stop();
        }
    }

    pub fn has_input(&self, index: usize) -> bool {
        self.controls.iter().any(|(slot, _)| *slot == index)
    }

    pub fn song_selected(&self, index: usize) -> bool {
        self.controls
            .iter()
            .any(|(slot, control)| *slot == index && control.selection == InputSelection::File)
    }

    /// `song` names the speaker's song, offered beside apps and devices.
    pub fn input_picker(
        &mut self,
        ui: &mut eframe::egui::Ui,
        index: usize,
        editable: bool,
        song: Option<&str>,
    ) -> bool {
        let Some((_, control)) = self.controls.iter_mut().find(|(slot, _)| *slot == index) else {
            return false;
        };
        let mut selection = control.selection.clone();
        let song_label = song.map(|song| format!("Song · {song}"));
        ui.horizontal(|ui| {
            ui.small("Music from");
            ui.add_enabled_ui(editable, |ui| {
                eframe::egui::ComboBox::from_id_salt(("app-audio", index))
                    .selected_text(match (&selection, &song_label) {
                        (InputSelection::File, Some(label)) => label.as_str(),
                        (selection, _) => selection.label(),
                    })
                    .show_ui(ui, |ui| {
                        if let Some(label) = &song_label {
                            ui.selectable_value(&mut selection, InputSelection::File, label);
                        }
                        ui.selectable_value(
                            &mut selection,
                            InputSelection::All,
                            "All system audio",
                        );
                        for app in &self.apps {
                            ui.selectable_value(
                                &mut selection,
                                InputSelection::App(app.clone()),
                                &app.label,
                            );
                        }
                        ui.selectable_value(
                            &mut selection,
                            InputSelection::Device("BlackHole 2ch".into()),
                            "BlackHole 2ch (fallback)",
                        );
                        if let InputSelection::Device(name) = &control.selection
                            && name != "BlackHole 2ch"
                        {
                            ui.selectable_value(&mut selection, control.selection.clone(), name);
                        }
                    });
                if ui.button("Refresh").clicked() && self.app_list.is_none() {
                    self.app_list = Some(crate::app_tap::enumerate());
                }
            });
        });
        let changed = selection != control.selection;
        if changed {
            control.stop();
            control.selection = selection;
            control.message = "Ready · press Play".into();
        }
        ui.small(&control.message);
        if let Some(error) = &self.app_list_error {
            ui.small(error);
        }
        changed
    }
}

impl Deref for LiveAudio {
    type Target = LiveOutput;
    fn deref(&self) -> &Self::Target {
        &self.output
    }
}

pub struct PreparedInputs {
    pub readers: Vec<Option<InputReader>>,
    pub lifetimes: Vec<Box<dyn Any>>,
    pub telemetry: Vec<(usize, LiveInputTelemetryReader)>,
    pub output_device: Option<String>,
    pub controls: Vec<(usize, InputControl)>,
}

pub fn prepare(
    devices: &[Option<String>],
    wav: Option<&Path>,
    output_rate: u32,
    output: Option<&str>,
    null_output: bool,
) -> Result<PreparedInputs, String> {
    let live_count = devices.iter().flatten().count();
    if wav.is_some() && live_count != 1 {
        return Err("--live-input-wav requires exactly one fixture live_input source".into());
    }
    let output_device = if live_count > 0 && !null_output {
        Some(resolved_output_device_name(output)?)
    } else {
        None
    };
    let mut prepared = PreparedInputs {
        readers: Vec::with_capacity(devices.len()),
        lifetimes: Vec::new(),
        telemetry: Vec::new(),
        output_device,
        controls: Vec::new(),
    };
    for (index, device) in devices.iter().enumerate() {
        let Some(device) = device else {
            prepared.readers.push(None);
            continue;
        };
        if let Some(path) = wav {
            let (rate, samples) = crate::asset::load_live_input_wav(path)?;
            let (producer, consumer, telemetry) = stereo_ring(rate);
            let feeder = WavFeeder::start(producer, samples, rate)?;
            let (mut writer, reader) = input_channel();
            writer.publish(Some(AdaptiveInput::new(consumer, rate, output_rate)));
            prepared.readers.push(Some(InputReader::Switching(reader)));
            prepared.lifetimes.push(Box::new(writer));
            prepared.lifetimes.push(Box::new(feeder));
            prepared.telemetry.push((index, telemetry));
        } else {
            let (writer, reader) = input_channel();
            prepared.readers.push(Some(InputReader::Switching(reader)));
            prepared.controls.push((
                index,
                InputControl {
                    writer,
                    selection: if device == "All system audio" {
                        InputSelection::All
                    } else {
                        InputSelection::Device(device.clone())
                    },
                    helper: None,
                    capture: None,
                    telemetry: None,
                    output_rate,
                    output_device: prepared.output_device.clone(),
                    null_output,
                    message: "Ready · press Play".into(),
                },
            ));
        }
    }
    Ok(prepared)
}

#[derive(Clone, PartialEq, Eq)]
enum InputSelection {
    All,
    App(AudioApp),
    Device(String),
    File,
}

impl InputSelection {
    fn label(&self) -> &str {
        match self {
            Self::All => "All system audio",
            Self::App(app) => &app.label,
            Self::Device(name) => name,
            Self::File => "Song file",
        }
    }
}

pub struct InputControl {
    writer: InputWriter,
    selection: InputSelection,
    helper: Option<TapHelper>,
    capture: Option<Box<dyn Any>>,
    telemetry: Option<LiveInputTelemetryReader>,
    output_rate: u32,
    output_device: Option<String>,
    null_output: bool,
    message: String,
}

impl InputControl {
    fn start(&mut self) -> Result<(), String> {
        if self.helper.is_some() || self.capture.is_some() {
            return Ok(());
        }
        let result = self.start_selected();
        if let Err(error) = &result {
            self.message = error.clone();
        }
        result
    }

    fn start_selected(&mut self) -> Result<(), String> {
        if self.null_output {
            return Err(
                "Device-free output cannot capture apps or input devices; use --live-input-wav"
                    .into(),
            );
        }
        match &self.selection {
            InputSelection::File => {
                return Err("Choose an app or drop a song, then press Play".into());
            }
            InputSelection::Device(device) => {
                feedback_guard(
                    device,
                    self.output_device.as_deref().ok_or("No output device")?,
                )?;
                let (stream, input, telemetry) = LiveCapture::open(device, self.output_rate)?;
                self.capture = Some(Box::new(stream));
                self.telemetry = Some(telemetry);
                self.writer.publish(Some(input));
                self.message = "Live input · playing through the city".into();
            }
            selection => {
                let processes = match selection {
                    InputSelection::App(app) => Some(app.processes.as_slice()),
                    _ => None,
                };
                self.helper = Some(TapHelper::start(processes, self.output_rate)?);
                self.message = TapStatus::Starting.label().into();
            }
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.writer.publish(None);
        self.helper = None;
        self.capture = None;
        self.telemetry = None;
        self.message = TapStatus::Stopped.label().into();
    }

    fn poll(&mut self) {
        let mut terminal = false;
        while let Some(event) = self
            .helper
            .as_ref()
            .and_then(|helper| helper.events.try_recv().ok())
        {
            terminal |= self.apply_event(event);
        }
        if terminal {
            self.writer.publish(None);
            self.helper = None;
            self.telemetry = None;
        }
        self.writer.reclaim();
    }

    fn apply_event(&mut self, event: Event) -> bool {
        match event {
            Event::Input(input, telemetry) => {
                self.writer.publish(Some(input));
                self.telemetry = Some(telemetry);
                false
            }
            Event::Status(status) => {
                self.message = status.label().into();
                matches!(
                    status,
                    TapStatus::Denied | TapStatus::Error | TapStatus::Stopped
                )
            }
        }
    }
}

// Three banks keep both adoption and retired-ring destruction off the callback.
struct InputBanks {
    banks: [UnsafeCell<Option<AdaptiveInput>>; 3],
    state: AtomicUsize,
}

// SAFETY: one control writer and one audio reader; the atomic state marks both
// the published bank and the bank exclusively owned by the audio reader.
unsafe impl Sync for InputBanks {}

struct InputWriter {
    shared: Arc<InputBanks>,
}
pub struct SwitchingInput {
    shared: Arc<InputBanks>,
    slot: usize,
}

fn input_channel() -> (InputWriter, SwitchingInput) {
    let shared = Arc::new(InputBanks {
        banks: std::array::from_fn(|_| UnsafeCell::new(None)),
        state: AtomicUsize::new(0),
    });
    (
        InputWriter {
            shared: Arc::clone(&shared),
        },
        SwitchingInput { shared, slot: 0 },
    )
}

impl InputWriter {
    fn publish(&mut self, input: Option<AdaptiveInput>) {
        let mut state = self.shared.state.load(Ordering::Acquire);
        let slot = (0..3)
            .find(|slot| *slot != (state & 3) && *slot != ((state >> 2) & 3))
            .unwrap();
        // SAFETY: the free bank cannot be adopted before this publication.
        unsafe {
            *self.shared.banks[slot].get() = input;
        }
        loop {
            match self.shared.state.compare_exchange(
                state,
                slot | (state & 12),
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => state = observed,
            }
        }
    }

    fn reclaim(&mut self) {
        let state = self.shared.state.load(Ordering::Acquire);
        for slot in 0..3 {
            if slot != (state & 3) && slot != ((state >> 2) & 3) {
                // SAFETY: only the published bank can become the reading bank.
                unsafe {
                    *self.shared.banks[slot].get() = None;
                }
            }
        }
    }
}

impl SwitchingInput {
    fn input(&mut self) -> Option<&mut AdaptiveInput> {
        let state = self.shared.state.load(Ordering::Acquire);
        let published = state & 3;
        if published != self.slot
            && self
                .shared
                .state
                .compare_exchange(
                    state,
                    published | (published << 2),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            self.slot = published;
        }
        // SAFETY: this bank is exclusively marked reading until next adoption;
        // the mutable borrow prevents adoption during its use.
        unsafe { &mut *self.shared.banks[self.slot].get() }.as_mut()
    }
}

pub enum InputReader {
    Fixed(AdaptiveInput),
    Switching(SwitchingInput),
}

impl From<AdaptiveInput> for InputReader {
    fn from(input: AdaptiveInput) -> Self {
        Self::Fixed(input)
    }
}

impl InputReader {
    fn input(&mut self) -> Option<&mut AdaptiveInput> {
        match self {
            Self::Fixed(input) => Some(input),
            Self::Switching(reader) => reader.input(),
        }
    }
    pub fn fill_block(&mut self, output: &mut [f32]) {
        if let Some(input) = self.input() {
            input.fill_block(output);
        } else {
            output.fill(0.0);
        }
    }
    pub fn fill_stereo_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        if let Some(input) = self.input() {
            input.fill_stereo_block(left, right);
        } else {
            left.fill(0.0);
            right.fill(0.0);
        }
    }
}

struct WavFeeder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WavFeeder {
    fn start(
        mut producer: StereoProducer,
        samples: Vec<[f32; 2]>,
        rate: u32,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("live-input-wav".into())
            .spawn(move || {
                let started = Instant::now();
                let chunk = (rate / 100) as usize;
                let mut frames = 0_u64;
                let mut packet = vec![0; chunk * 8];
                while !stopped.load(Ordering::Acquire) {
                    for frame in packet.chunks_exact_mut(8) {
                        let [left, right] = samples[frames as usize % samples.len()];
                        frame[..4].copy_from_slice(&left.to_le_bytes());
                        frame[4..].copy_from_slice(&right.to_le_bytes());
                        frames += 1;
                    }
                    crate::app_tap::feed_pcm(&mut producer, &packet)
                        .expect("complete synthetic stereo packet");
                    let deadline =
                        started + Duration::from_secs_f64(frames as f64 / f64::from(rate));
                    while !stopped.load(Ordering::Acquire) {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        std::thread::sleep((deadline - now).min(Duration::from_millis(10)));
                    }
                }
            })
            .map_err(|error| format!("cannot start test WAV feeder: {error}"))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for WavFeeder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_tap_input_switch_and_stop_allocate_nothing_on_callback() {
        let (mut writer, reader) = input_channel();
        let mut reader = InputReader::Switching(reader);
        let mut left = [1.0; 256];
        let mut right = [1.0; 256];
        reader.fill_stereo_block(&mut left, &mut right);
        assert_eq!(left, [0.0; 256]);
        for rate in [44_100, 48_000] {
            let (mut producer, consumer, _) = stereo_ring(rate);
            for _ in 0..rate / 5 {
                producer.push([0.25, -0.5]);
            }
            writer.publish(Some(AdaptiveInput::new(consumer, rate, 48_000)));
            let calls = crate::ballistic_crack::tests::count_allocator_calls(|| {
                reader.fill_stereo_block(&mut left, &mut right)
            });
            assert_eq!(calls, (0, 0));
            assert_eq!(left[255], 0.25);
            assert_eq!(right[255], -0.5);
            writer.reclaim();
        }
        writer.publish(None);
        assert_eq!(
            crate::ballistic_crack::tests::count_allocator_calls(
                || reader.fill_stereo_block(&mut left, &mut right)
            ),
            (0, 0)
        );
        assert_eq!(left, [0.0; 256]);
        assert_eq!(right, [0.0; 256]);
        writer.reclaim();
    }

    #[test]
    fn app_tap_denial_is_one_panel_message_and_null_output_cannot_capture() {
        let mut prepared =
            prepare(&[Some("All system audio".into())], None, 48_000, None, true).unwrap();
        let control = &mut prepared.controls[0].1;
        assert!(control.apply_event(Event::Status(TapStatus::Denied)));
        assert!(control.message.contains("System Audio Recording denied"));
        assert!(control.helper.is_none());
        assert!(control.capture.is_none());
        assert!(
            control
                .start()
                .unwrap_err()
                .contains("Device-free output cannot capture")
        );
        assert!(control.helper.is_none());
        assert!(control.capture.is_none());
    }
}
