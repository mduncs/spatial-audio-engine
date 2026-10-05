use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fightbox_runtime::live_input::{
    AdaptiveInput, LiveInputTelemetryReader, StereoProducer, stereo_ring,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct AudioApp {
    pub label: String,
    pub processes: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TapStatus {
    Starting,
    Ready,
    Denied,
    Error,
    Stopped,
}

impl TapStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "Waiting for System Audio Recording access…",
            Self::Ready => "App audio · playing through the city",
            Self::Denied => {
                "System Audio Recording denied · allow Fightbox City Audio in System Settings → Privacy & Security"
            }
            Self::Error => {
                "App audio unavailable · check System Audio Recording access or choose BlackHole"
            }
            Self::Stopped => "Stopped · app audio restored",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Header {
    Status(TapStatus),
    Format(u32),
    Pcm { rate: u32, frames: usize },
}

impl Header {
    fn parse(bytes: &[u8; 16]) -> Result<Self, String> {
        if &bytes[..4] != b"FAT1" {
            return Err("invalid app audio protocol".into());
        }
        let number = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let (kind, value, count) = (number(4), number(8), number(12));
        match (kind, value, count) {
            (0, status, 0) => Ok(Self::Status(match status {
                0 => TapStatus::Starting,
                1 => TapStatus::Ready,
                2 => TapStatus::Denied,
                3 => TapStatus::Error,
                4 => TapStatus::Stopped,
                _ => return Err("invalid app audio status".into()),
            })),
            (2, rate @ 8_000..=384_000, 2) => Ok(Self::Format(rate)),
            (1, rate @ 8_000..=384_000, frames @ 1..=4096) => Ok(Self::Pcm {
                rate,
                frames: frames as usize,
            }),
            _ => Err("invalid app audio format or packet size".into()),
        }
    }
}

pub(crate) fn feed_pcm(producer: &mut StereoProducer, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() % 8 != 0 {
        return Err("truncated app stereo frame".into());
    }
    for frame in bytes.chunks_exact(8) {
        producer.push([
            f32::from_le_bytes(frame[..4].try_into().unwrap()),
            f32::from_le_bytes(frame[4..].try_into().unwrap()),
        ]);
    }
    Ok(())
}

pub(crate) enum Event {
    Input(AdaptiveInput, LiveInputTelemetryReader),
    Status(TapStatus),
}

pub(crate) struct TapHelper {
    child: Child,
    thread: Option<JoinHandle<()>>,
    pub events: Receiver<Event>,
}

impl TapHelper {
    pub fn start(processes: Option<&[u32]>, output_rate: u32) -> Result<Self, String> {
        let binary = helper_binary()?;
        let selection = processes.map_or_else(
            || "all".into(),
            |ids| ids.iter().map(u32::to_string).collect::<Vec<_>>().join(","),
        );
        let mut child = Command::new(binary)
            .args([
                "--capture",
                &selection,
                "--exclude-pid",
                &std::process::id().to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("cannot start app audio helper: {error}"))?;
        let mut stdout = child.stdout.take().expect("piped helper stdout");
        let (sender, events) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("app-audio-pcm".into())
            .spawn(move || {
                if let Err(error) = read_stream(&mut stdout, output_rate, &sender) {
                    eprintln!("[app audio] {error}");
                    let _ = sender.send(Event::Status(TapStatus::Error));
                }
            });
        match thread {
            Ok(thread) => Ok(Self {
                child,
                thread: Some(thread),
                events,
            }),
            Err(error) => {
                stop_child(&mut child);
                Err(format!("cannot read app audio: {error}"))
            }
        }
    }
}

fn read_stream(
    reader: &mut impl Read,
    output_rate: u32,
    sender: &Sender<Event>,
) -> Result<(), String> {
    let mut header = [0; 16];
    let mut pcm = [0; 4096 * 8];
    let mut input: Option<(u32, StereoProducer)> = None;
    loop {
        reader
            .read_exact(&mut header)
            .map_err(|error| format!("app audio helper stopped: {error}"))?;
        match Header::parse(&header)? {
            Header::Status(status) => {
                sender
                    .send(Event::Status(status))
                    .map_err(|_| "app audio stopped")?;
                if matches!(
                    status,
                    TapStatus::Denied | TapStatus::Error | TapStatus::Stopped
                ) {
                    return Ok(());
                }
            }
            Header::Format(rate) => {
                if input.is_some() {
                    return Err("app audio format changed; press Play again".into());
                }
                let (producer, consumer, telemetry) = stereo_ring(rate);
                sender
                    .send(Event::Input(
                        AdaptiveInput::new(consumer, rate, output_rate),
                        telemetry,
                    ))
                    .map_err(|_| "app audio stopped")?;
                input = Some((rate, producer));
            }
            Header::Pcm { rate, frames } => {
                let (native_rate, producer) =
                    input.as_mut().ok_or("app audio PCM before format")?;
                if *native_rate != rate {
                    return Err("app audio rate changed; press Play again".into());
                }
                reader
                    .read_exact(&mut pcm[..frames * 8])
                    .map_err(|_| "truncated app audio packet")?;
                feed_pcm(producer, &pcm[..frames * 8])?;
            }
        }
    }
}

impl Drop for TapHelper {
    fn drop(&mut self) {
        stop_child(&mut self.child);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn stop_child(child: &mut Child) {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"stop\n");
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn helper_binary() -> Result<PathBuf, String> {
    if !cfg!(target_os = "macos") {
        return Err("App audio requires macOS 14.2 or later".into());
    }
    let suffix = "app-tap/FightboxAudioTap.app/Contents/MacOS/FightboxAudioTap";
    let alongside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent().map(|path| path.join(suffix)));
    let path = alongside.filter(|path| path.is_file()).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(suffix)
    });
    if !path.is_file() {
        return Err(
            "App audio helper missing · run tools/fightbox-workbench/app-tap/build.sh".into(),
        );
    }
    Ok(path)
}

pub(crate) fn enumerate() -> Receiver<Result<Vec<AudioApp>, String>> {
    let (sender, receiver) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("audio-app-list".into())
        .spawn(move || {
            let result = helper_binary().and_then(|binary| {
                let output = Command::new(binary)
                    .args(["--list", "--exclude-pid", &std::process::id().to_string()])
                    .output()
                    .map_err(|error| error.to_string())?;
                if !output.status.success() {
                    return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
                }
                serde_json::from_slice(&output.stdout)
                    .map_err(|error| format!("cannot list audio apps: {error}"))
            });
            let _ = sender.send(result);
        });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: u32, value: u32, count: u32) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[..4].copy_from_slice(b"FAT1");
        bytes[4..8].copy_from_slice(&kind.to_le_bytes());
        bytes[8..12].copy_from_slice(&value.to_le_bytes());
        bytes[12..].copy_from_slice(&count.to_le_bytes());
        bytes
    }

    #[test]
    fn app_tap_protocol_validates_framing_and_denial_without_capture() {
        assert_eq!(
            Header::parse(&header(2, 44_100, 2)),
            Ok(Header::Format(44_100))
        );
        assert!(Header::parse(&header(1, 48_000, 4097)).is_err());
        assert!(Header::parse(&header(2, 0, 2)).is_err());
        let (sender, receiver) = mpsc::channel();
        read_stream(&mut &header(0, 2, 0)[..], 48_000, &sender).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Ok(Event::Status(TapStatus::Denied))
        ));
        assert!(
            TapStatus::Denied
                .label()
                .contains("System Audio Recording denied")
        );
        assert!(receiver.try_recv().is_err());
        assert!(read_stream(&mut &header(1, 48_000, 1)[..], 48_000, &sender).is_err());
    }

    #[test]
    fn app_tap_pcm_keeps_stereo_level_and_sanitizes_nonfinite_samples() {
        let (mut producer, mut consumer, _) = stereo_ring(44_100);
        let samples: [f32; 6] = [0.25, -0.5, f32::NAN, f32::INFINITY, -0.125, 0.375];
        let bytes: Vec<_> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        feed_pcm(&mut producer, &bytes).unwrap();
        assert_eq!(consumer.pop(), Some([0.25, -0.5]));
        assert_eq!(consumer.pop(), Some([0.0, 0.0]));
        assert_eq!(consumer.pop(), Some([-0.125, 0.375]));
        assert!(feed_pcm(&mut producer, &bytes[..7]).is_err());
    }
}
