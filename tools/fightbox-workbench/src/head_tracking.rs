use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fightbox_api::{EnuVector3, ListenerState};
use fightbox_runtime::{SnapshotPublication, SnapshotReader};

const RECORD_BYTES: usize = 48;
const STALE_AFTER: Duration = Duration::from_millis(500);
const EASE_BACK_SECONDS: f32 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Quaternion([f32; 4]);

impl Quaternion {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 0.0]);

    fn inverse(self) -> Self {
        let [w, x, y, z] = self.0;
        Self([w, -x, -y, -z])
    }

    fn compose(self, other: Self) -> Self {
        let [w, x, y, z] = self.0;
        let [v, a, b, c] = other.0;
        Self([
            w * v - x * a - y * b - z * c,
            w * a + x * v + y * c - z * b,
            w * b - x * c + y * v + z * a,
            w * c + x * b - y * a + z * v,
        ])
    }

    fn rotate(self, vector: EnuVector3) -> EnuVector3 {
        let rotated = self
            .compose(Self([0.0, vector.east_m, vector.north_m, vector.up_m]))
            .compose(self.inverse());
        EnuVector3::new(rotated.0[1], rotated.0[2], rotated.0[3])
    }

    fn toward_identity(self, amount: f32) -> Self {
        let mut q = self.0;
        if q[0] < 0.0 {
            q = q.map(|value| -value);
        }
        let angle = q[0].clamp(-1.0, 1.0).acos();
        if angle < 1.0e-5 {
            return Self::IDENTITY;
        }
        let remaining = angle * (1.0 - amount.clamp(0.0, 1.0));
        let scale = remaining.sin() / angle.sin();
        Self([remaining.cos(), q[1] * scale, q[2] * scale, q[3] * scale])
    }
}

fn core_motion_to_enu(reference: Quaternion, current: Quaternion) -> Quaternion {
    // AirPods probe 2026-10-01: attitude is the active head rotation (+x right,
    // +y forward, +z up) in a gravity-vertical frame with arbitrary heading.
    // The world-frame delta keeps pitch/roll about gravity; the reference nose
    // heading becomes north. MotionMapping.swift ports this; mapping-cases.tsv
    // pins both. Steam's ENU -> (east,up,-north) mapping stays in the backend.
    let heading = reference_heading(reference);
    heading
        .compose(current.compose(reference.inverse()))
        .compose(heading.inverse())
}

fn reference_heading(reference: Quaternion) -> Quaternion {
    // Yaw turning the reference nose to north; the right ear covers a near-
    // vertical nose.
    let forward = reference.rotate(EnuVector3::new(0.0, 1.0, 0.0));
    let (east, north) = if forward.east_m.hypot(forward.north_m) >= 0.2 {
        (forward.east_m, forward.north_m)
    } else {
        let right = reference.rotate(EnuVector3::new(1.0, 0.0, 0.0));
        (-right.north_m, right.east_m)
    };
    let half_turn = 0.5 * (std::f32::consts::FRAC_PI_2 - north.atan2(east));
    Quaternion([half_turn.cos(), 0.0, 0.0, half_turn.sin()])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Waiting,
    Tracking,
    Denied,
    NoHeadphones,
    Unsupported,
    AuthorizedWaiting,
    Error,
    Stopped,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting for motion access",
            Self::Tracking => "authorized · tracking",
            Self::Denied => "denied · allow motion access in System Settings",
            Self::NoHeadphones => "no headphones · connect motion-capable AirPods",
            Self::Unsupported => "not supported",
            Self::AuthorizedWaiting => "authorized · waiting for motion",
            Self::Error => "headphone motion error · see probe verdict",
            Self::Stopped => "helper stopped or invalid motion record · run probe",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Record {
    status: Status,
    timestamp: f64,
    attitude: Quaternion,
}

impl Record {
    // FHT1, u32 status, f64 uptime, f64 w/x/y/z; all numbers little endian.
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != RECORD_BYTES || &bytes[..4] != b"FHT1" {
            return None;
        }
        let status = match u32::from_le_bytes(bytes[4..8].try_into().ok()?) {
            0 => Status::Waiting,
            1 => Status::Tracking,
            2 => Status::Denied,
            3 => Status::NoHeadphones,
            4 => Status::Unsupported,
            5 => Status::AuthorizedWaiting,
            6 => Status::Error,
            _ => return None,
        };
        let number = |offset| {
            Some(f64::from_le_bytes(
                bytes[offset..offset + 8].try_into().ok()?,
            ))
        };
        let timestamp = number(8)?;
        let q = [number(16)?, number(24)?, number(32)?, number(40)?];
        let norm = q.iter().map(|value| value * value).sum::<f64>().sqrt();
        if !timestamp.is_finite() || timestamp < 0.0 || !(0.5..=1.5).contains(&norm) {
            return None;
        }
        Some(Self {
            status,
            timestamp,
            attitude: Quaternion(q.map(|value| (value / norm) as f32)),
        })
    }

    fn sample(self, now: Instant, uptime: f64) -> Option<Sample> {
        let mut received = now;
        if self.status == Status::Tracking {
            let age = uptime - self.timestamp;
            if !(-0.1..=STALE_AFTER.as_secs_f64()).contains(&age) {
                return None;
            }
            received -= Duration::from_secs_f64(age.max(0.0));
        }
        Some(Sample {
            record: self,
            received,
        })
    }
}

#[derive(Clone, Copy)]
struct Sample {
    record: Record,
    received: Instant,
}

struct Helper {
    child: Child,
    reader: SnapshotReader<Sample>,
    thread: Option<JoinHandle<()>>,
}

impl Helper {
    #[cfg(target_os = "macos")]
    fn start() -> Result<Self, String> {
        let binary = helper_binary()
            .ok_or("helper missing · double-click run-headtracking-probe.command")?;
        let mut child = Command::new(binary)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("cannot start head tracker: {error}"))?;
        let mut stdout = child.stdout.take().expect("piped helper stdout");
        let (mut writer, reader) = SnapshotPublication::new(Sample {
            record: Record {
                status: Status::Waiting,
                timestamp: 0.0,
                attitude: Quaternion::IDENTITY,
            },
            received: Instant::now(),
        });
        let reader_thread = thread::Builder::new()
            .name("head-tracker".to_owned())
            .spawn(move || {
                let uptime_scale = uptime_scale();
                let mut bytes = [0_u8; RECORD_BYTES];
                while stdout.read_exact(&mut bytes).is_ok() {
                    let Some(record) = Record::parse(&bytes) else {
                        break;
                    };
                    // CoreMotion timestamps and ProcessInfo.systemUptime use
                    // mach_absolute_time. Reject a delayed pipe backlog too.
                    if let Some(sample) =
                        record.sample(Instant::now(), uptime_seconds(uptime_scale))
                    {
                        writer.publish(sample);
                    }
                }
                writer.publish(Sample {
                    record: Record {
                        status: Status::Stopped,
                        timestamp: 0.0,
                        attitude: Quaternion::IDENTITY,
                    },
                    received: Instant::now(),
                });
            });
        match reader_thread {
            Ok(reader_thread) => Ok(Self {
                child,
                reader,
                thread: Some(reader_thread),
            }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(format!("cannot read head tracker: {error}"))
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn start() -> Result<Self, String> {
        Err("not supported · requires macOS 14 or later".to_owned())
    }
}

#[cfg(target_os = "macos")]
fn uptime_scale() -> f64 {
    #[repr(C)]
    struct Timebase {
        numerator: u32,
        denominator: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    let mut timebase = Timebase {
        numerator: 0,
        denominator: 0,
    };
    // SAFETY: Darwin writes the two u32 fields into this valid local structure.
    let result = unsafe { mach_timebase_info(&mut timebase) };
    assert_eq!(result, 0, "Darwin monotonic clock timebase");
    f64::from(timebase.numerator) / f64::from(timebase.denominator) * 1.0e-9
}

#[cfg(target_os = "macos")]
fn uptime_seconds(scale: f64) -> f64 {
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
    }
    // SAFETY: Darwin's monotonic clock has no arguments or memory access.
    unsafe { mach_absolute_time() as f64 * scale }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader_thread) = self.thread.take() {
            let _ = reader_thread.join();
        }
    }
}

#[cfg(target_os = "macos")]
fn helper_binary() -> Option<PathBuf> {
    let suffix = "HeadTracker.app/Contents/MacOS/HeadTracker";
    if let Some(path) = std::env::var_os("FIGHTBOX_HEADTRACKER_APP") {
        let path = PathBuf::from(path).join("Contents/MacOS/HeadTracker");
        return path.is_file().then_some(path);
    }
    if let Ok(executable) = std::env::current_exe() {
        // target/{debug,release}/fightbox-workbench and target/HeadTracker.app.
        if let Some(target) = executable.parent().and_then(|directory| directory.parent()) {
            let path = target.join(suffix);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(suffix);
    path.is_file().then_some(path)
}

#[derive(Default)]
struct HeadPose {
    current: Option<Quaternion>,
    reference: Option<Quaternion>,
    last_received: Option<Instant>,
    last_timestamp: Option<f64>,
}

impl HeadPose {
    fn observe(&mut self, sample: Sample) {
        if sample.record.status != Status::Tracking
            || self
                .last_timestamp
                .is_some_and(|last| sample.record.timestamp <= last)
        {
            return;
        }
        self.last_timestamp = Some(sample.record.timestamp);
        self.last_received = Some(sample.received);
        self.current = Some(sample.record.attitude);
        self.reference.get_or_insert(sample.record.attitude);
    }

    fn recenter(&mut self) {
        self.reference = self.current;
    }

    fn age(&self, now: Instant) -> Option<Duration> {
        self.last_received
            .map(|received| now.saturating_duration_since(received))
    }

    fn rotation(&self, now: Instant) -> Quaternion {
        let (Some(current), Some(reference), Some(age)) =
            (self.current, self.reference, self.age(now))
        else {
            return Quaternion::IDENTITY;
        };
        let head = core_motion_to_enu(reference, current);
        let amount = age.saturating_sub(STALE_AFTER).as_secs_f32() / EASE_BACK_SECONDS;
        head.toward_identity(amount)
    }
}

#[derive(Default)]
pub(crate) struct HeadTracking {
    enabled: bool,
    helper: Option<Helper>,
    pose: HeadPose,
    status: Option<Status>,
    error: Option<String>,
}

impl HeadTracking {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.helper = None;
        self.pose = HeadPose::default();
        self.status = None;
        self.error = None;
        self.enabled = enabled;
        if enabled {
            match Helper::start() {
                Ok(helper) => self.helper = Some(helper),
                Err(error) => self.error = Some(error),
            }
        }
    }

    pub fn can_recenter(&self) -> bool {
        self.enabled
            && self
                .pose
                .age(Instant::now())
                .is_some_and(|age| age <= STALE_AFTER)
    }

    pub fn recenter(&mut self) {
        if self.can_recenter() {
            self.pose.recenter();
        }
    }

    pub fn status(&self) -> &str {
        if let Some(error) = &self.error {
            return error;
        }
        if self.status == Some(Status::Tracking)
            && self
                .pose
                .age(Instant::now())
                .is_some_and(|age| age > STALE_AFTER)
        {
            return "stale motion · returning to body direction";
        }
        self.status.unwrap_or(Status::Waiting).label()
    }

    pub fn apply(&mut self, listener: &mut ListenerState, body_yaw: f32) {
        if !self.enabled {
            return;
        }
        if let Some(helper) = &mut self.helper {
            let sample = helper.reader.read();
            self.status = Some(sample.record.status);
            self.pose.observe(sample);
        }
        let head = self.pose.rotation(Instant::now());
        let half_yaw = -body_yaw * 0.5;
        let body = Quaternion([half_yaw.cos(), 0.0, 0.0, half_yaw.sin()]);
        let ears = body.compose(head);
        listener.pose.forward = ears.rotate(EnuVector3::new(0.0, 1.0, 0.0));
        listener.pose.up = ears.rotate(EnuVector3::new(0.0, 0.0, 1.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pose::ListenerControl;

    fn axis_rotation(axis: usize, angle: f32) -> Quaternion {
        let mut q = [(angle * 0.5).cos(), 0.0, 0.0, 0.0];
        q[axis] = (angle * 0.5).sin();
        Quaternion(q)
    }

    fn near(actual: EnuVector3, expected: EnuVector3) {
        assert!(
            (actual.east_m - expected.east_m).abs() < 1.0e-5,
            "{actual:?}"
        );
        assert!(
            (actual.north_m - expected.north_m).abs() < 1.0e-5,
            "{actual:?}"
        );
        assert!((actual.up_m - expected.up_m).abs() < 1.0e-5, "{actual:?}");
    }

    fn sample(attitude: Quaternion, timestamp: f64, received: Instant) -> Sample {
        Sample {
            record: Record {
                status: Status::Tracking,
                timestamp,
                attitude,
            },
            received,
        }
    }

    fn pose_for_physical_rotation(rotation: Quaternion, now: Instant) -> HeadPose {
        let mut pose = HeadPose::default();
        pose.observe(sample(Quaternion::IDENTITY, 1.0, now));
        // Synthetic inputs use the probe-verified active attitude convention.
        pose.observe(sample(rotation, 2.0, now));
        pose
    }

    #[test]
    fn headphone_axes_yaw_left_pitch_up_and_roll() {
        let now = Instant::now();
        let forward = EnuVector3::new(0.0, 1.0, 0.0);
        let ninety = std::f32::consts::FRAC_PI_2;
        let yaw = pose_for_physical_rotation(axis_rotation(3, ninety), now).rotation(now);
        near(yaw.rotate(forward), EnuVector3::new(-1.0, 0.0, 0.0));
        let pitch = pose_for_physical_rotation(axis_rotation(1, ninety), now).rotation(now);
        near(pitch.rotate(forward), EnuVector3::new(0.0, 0.0, 1.0));
        let roll = pose_for_physical_rotation(axis_rotation(2, ninety), now).rotation(now);
        near(roll.rotate(forward), forward);
        near(
            roll.rotate(EnuVector3::new(0.0, 0.0, 1.0)),
            EnuVector3::new(1.0, 0.0, 0.0),
        );
    }

    #[test]
    fn probe_and_workbench_share_numeric_mapping_cases() {
        let cases = include_str!("../../../platforms/macos/HeadTracker/mapping-cases.tsv");
        let mut count = 0;
        for line in cases
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
        {
            let fields = line.split('\t').collect::<Vec<_>>();
            assert_eq!(fields.len(), 12, "{}", fields[0]);
            let values = fields[1..]
                .iter()
                .map(|value| value.parse::<f64>().unwrap())
                .collect::<Vec<_>>();
            let normalized = |offset| {
                let q: [f64; 4] = values[offset..offset + 4].try_into().unwrap();
                let norm = q.iter().map(|value| value * value).sum::<f64>().sqrt();
                Quaternion(q.map(|value| (value / norm) as f32))
            };
            let head = core_motion_to_enu(normalized(0), normalized(4));
            near(
                head.rotate(EnuVector3::new(0.0, 1.0, 0.0)),
                EnuVector3::new(values[8] as f32, values[9] as f32, values[10] as f32),
            );
            count += 1;
        }
        assert_eq!(count, 9);
    }

    #[test]
    fn body_composes_head_and_movement_stays_with_body() {
        let now = Instant::now();
        let mut control =
            ListenerControl::at(EnuVector3::default(), EnuVector3::new(1.0, 0.0, 0.0));
        let velocity = control.walk(1.0, 0.0, false, 1.0);
        let mut listener = control.listener_state(velocity);
        let mut tracking = HeadTracking {
            enabled: true,
            pose: pose_for_physical_rotation(axis_rotation(1, std::f32::consts::FRAC_PI_4), now),
            ..Default::default()
        };
        tracking.apply(&mut listener, control.yaw_radians);
        let half = 0.5_f32.sqrt();
        near(listener.pose.forward, EnuVector3::new(half, 0.0, half));
        assert_eq!(listener.linear_velocity_mps, velocity);
        near(control.forward(), EnuVector3::new(1.0, 0.0, 0.0));
    }

    #[test]
    fn recenter_uses_full_reference_and_world_rotation_order() {
        let now = Instant::now();
        let reference = axis_rotation(2, 0.4).compose(axis_rotation(3, -0.7));
        let physical_delta = axis_rotation(3, std::f32::consts::FRAC_PI_2);
        let mut pose = HeadPose::default();
        pose.observe(sample(reference, 1.0, now));
        pose.recenter();
        near(
            pose.rotation(now).rotate(EnuVector3::new(0.0, 1.0, 0.0)),
            EnuVector3::new(0.0, 1.0, 0.0),
        );
        pose.observe(sample(
            physical_delta.compose(reference),
            2.0,
            now,
        ));
        near(
            pose.rotation(now).rotate(EnuVector3::new(0.0, 1.0, 0.0)),
            EnuVector3::new(-1.0, 0.0, 0.0),
        );
        pose.recenter();
        near(
            pose.rotation(now).rotate(EnuVector3::new(0.0, 1.0, 0.0)),
            EnuVector3::new(0.0, 1.0, 0.0),
        );
    }

    #[test]
    fn stale_data_eases_back_and_duplicate_records_do_not_refresh_it() {
        let now = Instant::now();
        let mut pose =
            pose_for_physical_rotation(axis_rotation(3, std::f32::consts::FRAC_PI_2), now);
        pose.observe(sample(
            Quaternion::IDENTITY,
            2.0,
            now + Duration::from_millis(400),
        ));
        let forward = EnuVector3::new(0.0, 1.0, 0.0);
        near(
            pose.rotation(now + Duration::from_millis(500))
                .rotate(forward),
            EnuVector3::new(-1.0, 0.0, 0.0),
        );
        let half = 0.5_f32.sqrt();
        near(
            pose.rotation(now + Duration::from_millis(650))
                .rotate(forward),
            EnuVector3::new(-half, half, 0.0),
        );
        near(
            pose.rotation(now + Duration::from_millis(800))
                .rotate(forward),
            forward,
        );
        pose.observe(sample(
            Quaternion::IDENTITY,
            3.0,
            now + Duration::from_millis(900),
        ));
        near(
            pose.rotation(now + Duration::from_millis(900))
                .rotate(forward),
            forward,
        );
        let delayed = sample(Quaternion::IDENTITY, 10.0, now).record;
        assert!(delayed.sample(now, 10.6).is_none());
        assert!(delayed.sample(now, 9.8).is_none());
        let accepted = delayed.sample(now, 10.25).unwrap();
        assert_eq!(
            now.duration_since(accepted.received),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn record_parser_checks_version_state_timestamp_and_quaternion() {
        let mut bytes = [0_u8; RECORD_BYTES];
        bytes[..4].copy_from_slice(b"FHT1");
        bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
        bytes[8..16].copy_from_slice(&123.5_f64.to_le_bytes());
        bytes[16..24].copy_from_slice(&1.0_f64.to_le_bytes());
        let parsed = Record::parse(&bytes).unwrap();
        assert_eq!(parsed.status, Status::Tracking);
        assert_eq!(parsed.timestamp, 123.5);
        assert_eq!(parsed.attitude, Quaternion::IDENTITY);
        assert!(Record::parse(&bytes[..47]).is_none());
        for state in 0_u32..=6 {
            bytes[4..8].copy_from_slice(&state.to_le_bytes());
            assert!(Record::parse(&bytes).is_some());
        }
        bytes[4..8].copy_from_slice(&7_u32.to_le_bytes());
        assert!(Record::parse(&bytes).is_none());
        bytes[4..8].copy_from_slice(&1_u32.to_le_bytes());
        bytes[8..16].copy_from_slice(&f64::NAN.to_le_bytes());
        assert!(Record::parse(&bytes).is_none());
        bytes[8..16].copy_from_slice(&1.0_f64.to_le_bytes());
        bytes[16..24].copy_from_slice(&0.0_f64.to_le_bytes());
        assert!(Record::parse(&bytes).is_none());
        bytes[16..24].copy_from_slice(&f64::INFINITY.to_le_bytes());
        assert!(Record::parse(&bytes).is_none());
        bytes[..4].copy_from_slice(b"FHT2");
        assert!(Record::parse(&bytes).is_none());
    }
}
