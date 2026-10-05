//! City Map link: an opt-in loopback channel between the Workbench and the
//! City Map app (`platforms/macos/CityMap`). One JSON object per line.
//!
//! Out: `hello` once per client (geo origin, quality dots, suggested spots,
//! sound ids, footprints, streets), then `state` at most 10 times a second,
//! `shot` for each acoustic-feed event (the crack, routed boom arrivals and
//! facade echoes the engine planned, in ENU metres and seconds from the
//! trigger), `notice` when a request is refused, and for music: `track`
//! (the song's own three-band envelope and kicks, once), `field` (each
//! walkable dot's routed band level from the speaker) and `paths` (the
//! routed and echo paths to You). In: `move`, `move_you`, `spot`, `set_on`,
//! `fire`, `select`, `play_all`, `stop_all`, `set_volume`.
//!
//! Everything runs on the UI thread with non-blocking sockets. Nothing here
//! touches the audio callback, the limiter or output safety; commands resolve
//! to the same Workbench actions as its own buttons.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ground_map::Point;

pub(crate) const ENV: &str = "FIGHTBOX_MAP_LINK";
pub(crate) const PROTOCOL: u32 = 1;
const MAX_CLIENTS: usize = 4;
const MAX_LINE_BYTES: usize = 64 * 1024;
/// A client that stops reading is dropped rather than buffered without end.
const MAX_BACKLOG_BYTES: usize = 16 * 1024 * 1024;
/// Map moves test coverage this far above the street or roof they land on;
/// height above that is free, as with the "roofline +3 m" height choice.
pub(crate) const MOVE_COVERAGE_TEST_M: f32 = 1.5;

/// One acoustic-feed event for the map, as the feed's own serde form.
/// `elapsed_s` is how far past the trigger the audio clock already is.
pub(crate) fn shot_json(event: &crate::acoustic_feed::AcousticEvent, elapsed_s: f64) -> String {
    serde_json::json!({
        "type": "shot",
        "elapsed_s": (elapsed_s.max(0.0) * 1000.0).round() / 1000.0,
        "event": event,
    })
    .to_string()
}

/// `FIGHTBOX_MAP_LINK` is a port or a loopback socket address.
pub(crate) fn parse_address(value: &str) -> Result<SocketAddr, String> {
    let value = value.trim();
    let address = match value.parse::<u16>() {
        Ok(port) => SocketAddr::from(([127, 0, 0, 1], port)),
        Err(_) => value
            .parse::<SocketAddr>()
            .map_err(|_| format!("{ENV}={value:?} is not a port or socket address"))?,
    };
    if !address.ip().is_loopback() {
        return Err(format!("{ENV} binds loopback only, not {}", address.ip()));
    }
    Ok(address)
}

struct Client {
    stream: TcpStream,
    inbox: Vec<u8>,
    outbox: Vec<u8>,
    greeted: bool,
    closed: bool,
}

impl Client {
    fn queue(&mut self, line: &str) {
        if self.outbox.len() + line.len() > MAX_BACKLOG_BYTES {
            self.closed = true;
            return;
        }
        self.outbox.extend_from_slice(line.as_bytes());
        self.outbox.push(b'\n');
    }

    fn flush(&mut self) {
        while !self.closed && !self.outbox.is_empty() {
            match self.stream.write(&self.outbox) {
                Ok(0) => self.closed = true,
                Ok(written) => {
                    self.outbox.drain(..written);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => self.closed = true,
            }
        }
    }

    fn read_lines(&mut self, lines: &mut Vec<String>) {
        let mut buffer = [0_u8; 4096];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    self.closed = true;
                    break;
                }
                Ok(read) => self.inbox.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
        }
        while let Some(end) = self.inbox.iter().position(|byte| *byte == b'\n') {
            let line = self.inbox.drain(..=end).collect::<Vec<_>>();
            let text = String::from_utf8_lossy(&line[..end]).trim().to_owned();
            if !text.is_empty() {
                lines.push(text);
            }
        }
        if self.inbox.len() > MAX_LINE_BYTES {
            self.closed = true;
        }
    }
}

/// The listening socket and its clients, polled once per UI frame.
pub(crate) struct MapLink {
    listener: TcpListener,
    clients: Vec<Client>,
    pub address: SocketAddr,
}

impl MapLink {
    pub(crate) fn from_env() -> Option<Result<Self, String>> {
        let value = std::env::var(ENV).ok()?;
        Some(parse_address(&value).and_then(|address| {
            Self::bind(address).map_err(|error| format!("{ENV}: cannot bind {address}: {error}"))
        }))
    }

    pub(crate) fn bind(address: SocketAddr) -> io::Result<Self> {
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the map link binds loopback only",
            ));
        }
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        Ok(Self {
            listener,
            clients: Vec::new(),
            address,
        })
    }

    /// Accepts waiting clients; true when one still needs the hello.
    pub(crate) fn accept(&mut self) -> bool {
        loop {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if !peer.ip().is_loopback() || self.clients.len() >= MAX_CLIENTS {
                        continue;
                    }
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let _ = stream.set_nodelay(true);
                    self.clients.push(Client {
                        stream,
                        inbox: Vec::new(),
                        outbox: Vec::new(),
                        greeted: false,
                        closed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        self.clients.iter().any(|client| !client.greeted)
    }

    pub(crate) fn greet(&mut self, hello: &str) {
        for client in self.clients.iter_mut().filter(|client| !client.greeted) {
            client.queue(hello);
            client.greeted = true;
        }
    }

    /// Sends the hello again, e.g. after a scene switch.
    pub(crate) fn regreet(&mut self) {
        for client in &mut self.clients {
            client.greeted = false;
        }
    }

    pub(crate) fn has_clients(&self) -> bool {
        !self.clients.is_empty()
    }

    /// Complete lines received since the last call, oldest first.
    pub(crate) fn read_commands(&mut self) -> Vec<Result<MapCommand, String>> {
        let mut lines = Vec::new();
        for client in self.clients.iter_mut().filter(|client| client.greeted) {
            client.read_lines(&mut lines);
        }
        lines.iter().map(|line| MapCommand::parse(line)).collect()
    }

    pub(crate) fn broadcast(&mut self, line: &str) {
        for client in self.clients.iter_mut().filter(|client| client.greeted) {
            client.queue(line);
        }
    }

    pub(crate) fn notice(&mut self, text: &str) {
        let line = serde_json::json!({ "type": "notice", "text": text }).to_string();
        self.broadcast(&line);
    }

    pub(crate) fn flush(&mut self) {
        for client in &mut self.clients {
            client.flush();
        }
        self.clients.retain(|client| !client.closed);
    }
}

/// One request from the map, by sound id.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MapCommand {
    /// Moves a static sound. Without `above_top_m` it keeps its height above
    /// the ground; `done` ends a drag (replan, then mark the scene dirty).
    Move {
        id: String,
        east_m: f32,
        north_m: f32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        above_top_m: Option<f32>,
        #[serde(default)]
        done: bool,
    },
    /// Puts a sound at one of the hello's suggested spots, by key. The
    /// Workbench owns the spot list, so the map can never invent a level.
    Spot {
        id: String,
        key: String,
    },
    SetOn {
        id: String,
        on: bool,
    },
    /// Plays a sound again from its start (artillery: one more shot).
    Fire {
        id: String,
    },
    /// Moves You to a tapped spot: snapped onto the nearest street or path,
    /// never inside a building.
    MoveYou {
        east_m: f32,
        north_m: f32,
    },
    Select {
        id: String,
    },
    PlayAll,
    StopAll,
    SetVolume {
        db: f32,
    },
}

/// A command checked against this scene: indices, finite and bounded values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MapAction {
    Move {
        index: usize,
        at: Point,
        above_top_m: Option<f32>,
        done: bool,
    },
    Spot {
        index: usize,
        spot: usize,
    },
    SetOn {
        index: usize,
        on: bool,
    },
    Fire {
        index: usize,
    },
    MoveYou {
        at: Point,
    },
    Select {
        index: usize,
    },
    PlayAll,
    StopAll,
    SetVolume {
        db: f32,
    },
}

impl MapCommand {
    pub(crate) fn parse(line: &str) -> Result<Self, String> {
        serde_json::from_str(line).map_err(|error| format!("unreadable map request: {error}"))
    }

    /// Resolves ids and spot keys and checks values. Volume clamps into the
    /// Workbench's own monitor-gain range; it never extends it.
    pub(crate) fn resolve(
        self,
        ids: &[&str],
        spot_keys: &[&str],
        volume_db: RangeInclusive<f32>,
    ) -> Result<MapAction, String> {
        let index = |id: &str| {
            ids.iter()
                .position(|candidate| *candidate == id)
                .ok_or_else(|| format!("no sound called {id:?}"))
        };
        Ok(match self {
            Self::Move {
                id,
                east_m,
                north_m,
                above_top_m,
                done,
            } => {
                if !(east_m.is_finite() && north_m.is_finite())
                    || east_m.abs() > 100_000.0
                    || north_m.abs() > 100_000.0
                {
                    return Err("that spot is not on this map".into());
                }
                if above_top_m.is_some_and(|above| !(0.0..=1000.0).contains(&above)) {
                    return Err("heights run from 0 to 1000 m above the street or roof".into());
                }
                MapAction::Move {
                    index: index(&id)?,
                    at: [east_m, north_m],
                    above_top_m,
                    done,
                }
            }
            Self::Spot { id, key } => MapAction::Spot {
                index: index(&id)?,
                spot: spot_keys
                    .iter()
                    .position(|candidate| *candidate == key)
                    .ok_or_else(|| format!("no suggested spot called {key:?}"))?,
            },
            Self::SetOn { id, on } => MapAction::SetOn {
                index: index(&id)?,
                on,
            },
            Self::Fire { id } => MapAction::Fire {
                index: index(&id)?,
            },
            Self::MoveYou { east_m, north_m } => {
                if !(east_m.is_finite() && north_m.is_finite())
                    || east_m.abs() > 100_000.0
                    || north_m.abs() > 100_000.0
                {
                    return Err("that spot is not on this map".into());
                }
                MapAction::MoveYou {
                    at: [east_m, north_m],
                }
            }
            Self::Select { id } => MapAction::Select {
                index: index(&id)?,
            },
            Self::PlayAll => MapAction::PlayAll,
            Self::StopAll => MapAction::StopAll,
            Self::SetVolume { db } => {
                if !db.is_finite() {
                    return Err("volume must be a number".into());
                }
                MapAction::SetVolume {
                    db: db.clamp(*volume_db.start(), *volume_db.end()),
                }
            }
        })
    }
}

/// Where a map move lands: `top_m` is the street or roof under the spot.
/// The spot must be baked just above that surface; the requested height
/// above it is free. `None` means no baked path there.
pub(crate) fn landing(
    at: Point,
    top_m: f32,
    above_top_m: f32,
    covered: impl Fn([f32; 3]) -> bool,
) -> Option<[f32; 3]> {
    let above = above_top_m.max(0.0);
    covered([at[0], at[1], top_m + above.min(MOVE_COVERAGE_TEST_M)])
        .then_some([at[0], at[1], top_m + above])
}

/// How far a tap may sit from a street, path or alley centreline.
pub(crate) const YOU_SNAP_M: f32 = 30.0;

/// Where a map tap puts You: the nearest point on a street, path or alley
/// centreline within [`YOU_SNAP_M`]. A tap inside a footprint, or one whose
/// snapped point is, is refused, as is one far from any street.
pub(crate) fn you_landing(
    tap: Point,
    access: &[Vec<Point>],
    roofs: &[[Point; 3]],
    footprints: &[Vec<Point>],
) -> Result<Point, String> {
    let inside = |point: Point| {
        roofs.iter().any(|roof| inside_triangle(point, roof))
            || footprints.iter().any(|ring| inside_ring(point, ring))
    };
    if inside(tap) {
        return Err("That's inside a building · tap a street".into());
    }
    let mut best: Option<(f32, Point)> = None;
    for line in access {
        for pair in line.windows(2) {
            let nearest = nearest_on_segment(tap, pair[0], pair[1]);
            let distance = ((tap[0] - nearest[0]).powi(2) + (tap[1] - nearest[1]).powi(2)).sqrt();
            if best.is_none_or(|(current, _)| distance < current) {
                best = Some((distance, nearest));
            }
        }
    }
    match best {
        Some((distance, at)) if distance <= YOU_SNAP_M && !inside(at) => {
            Ok([round(at[0], 10.0), round(at[1], 10.0)])
        }
        Some((distance, _)) if distance <= YOU_SNAP_M => {
            Err("That street runs under a building · tap another".into())
        }
        _ => Err("Tap a street or path to walk there".into()),
    }
}

fn nearest_on_segment(point: Point, a: Point, b: Point) -> Point {
    let along = [b[0] - a[0], b[1] - a[1]];
    let length = along[0] * along[0] + along[1] * along[1];
    let t = if length > 0.0 {
        (((point[0] - a[0]) * along[0] + (point[1] - a[1]) * along[1]) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    [a[0] + along[0] * t, a[1] + along[1] * t]
}

/// Drops marks standing inside a mapped building footprint (a tall block
/// whose roof the acoustic mesh lacks still hides its ground); returns how
/// many went.
pub(crate) fn drop_inside_footprints(field: &mut DotField, footprints: &[Vec<Point>]) -> usize {
    const CELL_M: f32 = 32.0;
    let key = |point: Point| ((point[0] / CELL_M).floor() as i32, (point[1] / CELL_M).floor() as i32);
    let mut cells = std::collections::HashMap::<(i32, i32), Vec<usize>>::new();
    for (index, ring) in footprints.iter().enumerate() {
        if ring.len() < 3 {
            continue;
        }
        let low = key([
            ring.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min),
            ring.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min),
        ]);
        let high = key([
            ring.iter().map(|p| p[0]).fold(f32::NEG_INFINITY, f32::max),
            ring.iter().map(|p| p[1]).fold(f32::NEG_INFINITY, f32::max),
        ]);
        for x in low.0..=high.0 {
            for y in low.1..=high.1 {
                cells.entry((x, y)).or_default().push(index);
            }
        }
    }
    let before = field.points.len();
    field.points.retain(|point| {
        let at = [point[0], point[1]];
        cells
            .get(&key(at))
            .is_none_or(|rings| !rings.iter().any(|ring| inside_ring(at, &footprints[*ring])))
    });
    let dropped = before - field.points.len();
    field.culled += dropped;
    dropped
}

pub(crate) fn inside_ring(point: Point, ring: &[Point]) -> bool {
    let mut inside = false;
    for index in 0..ring.len() {
        let (a, b) = (ring[index], ring[(index + 1) % ring.len()]);
        if (a[1] > point[1]) != (b[1] > point[1])
            && point[0] < (b[0] - a[0]) * (point[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
    }
    inside
}

/// A song's own three-band level over time (low < 150 Hz, mid, high
/// > 2.5 kHz), read once from its prepared samples on the control side,
/// plus the kicks found in its low band. The map plays it back against the
/// source's playhead; nothing here touches the audio callback.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct BandTrack {
    pub rate_hz: u32,
    pub length_s: f32,
    /// Interleaved `[low, mid, high]` dBFS, one triple per frame.
    pub bands_db: Vec<i8>,
    /// Kick onsets in seconds from the start of the song.
    pub kicks_s: Vec<f32>,
}

pub(crate) const TRACK_RATE_HZ: u32 = 30;

pub(crate) fn band_track(samples: &[f32], sample_rate_hz: u32) -> BandTrack {
    let rate = sample_rate_hz.max(1) as f32;
    let coefficient = |hz: f32| 1.0 - (-std::f32::consts::TAU * hz / rate).exp();
    let (low_k, high_k) = (coefficient(150.0), coefficient(2500.0));
    let frame = (sample_rate_hz / TRACK_RATE_HZ).max(1) as usize;
    // 10 ms low-band energy for kick timing.
    let fine = (sample_rate_hz / 100).max(1) as usize;
    let mut low = [0.0_f32; 2];
    let mut below_high = 0.0_f32;
    let mut sums = [0.0_f64; 3];
    let mut fine_sum = 0.0_f64;
    let mut bands_db = Vec::with_capacity(samples.len() / frame * 3 + 3);
    let mut fine_db = Vec::with_capacity(samples.len() / fine + 1);
    let db = |sum: f64, count: usize| {
        (10.0 * (sum / count.max(1) as f64 + 1.0e-12).log10()).clamp(-99.0, 0.0) as f32
    };
    for (index, &sample) in samples.iter().enumerate() {
        low[0] += low_k * (sample - low[0]);
        low[1] += low_k * (low[0] - low[1]);
        below_high += high_k * (sample - below_high);
        let bands = [low[1], below_high - low[1], sample - below_high];
        for (sum, band) in sums.iter_mut().zip(bands) {
            *sum += f64::from(band * band);
        }
        fine_sum += f64::from(low[1] * low[1]);
        if (index + 1) % fine == 0 {
            fine_db.push(db(fine_sum, fine));
            fine_sum = 0.0;
        }
        if (index + 1) % frame == 0 {
            bands_db.extend(sums.map(|sum| db(sum, frame).round() as i8));
            sums = [0.0; 3];
        }
    }
    // A kick: the low band jumps 6 dB over its last 80 ms within 30 ms, near
    // the loud end of the song's own low band, at most every 0.24 s.
    let mut sorted = fine_db.clone();
    sorted.sort_by(f32::total_cmp);
    let loud = sorted.get(sorted.len() * 9 / 10).copied().unwrap_or(0.0) - 9.0;
    let mut kicks_s = Vec::new();
    let mut last = f32::NEG_INFINITY;
    for index in 8..fine_db.len() {
        let before = fine_db[index - 8..index - 2].iter().copied().fold(f32::INFINITY, f32::min);
        let now = fine_db[index];
        let time = index as f32 / 100.0;
        if now >= loud && now - before >= 6.0 && time - last >= 0.24 {
            kicks_s.push(round(time - 0.01, 100.0));
            last = time;
        }
    }
    BandTrack {
        rate_hz: TRACK_RATE_HZ,
        length_s: samples.len() as f32 / rate,
        bands_db,
        kicks_s,
    }
}

/// A corner's band loss: bass wraps it, highs mostly do not. Pressure per
/// radian of turn, a coarse stand-in for edge diffraction.
const CORNER_LOSS_PER_RADIAN: [f32; 3] = [0.22, 0.85, 1.9];

/// Level of a routed path relative to 1 m from the speaker, per band, in
/// dB: spherical spreading over the routed length, the scene's air
/// absorption per band, and a loss at every corner it turns.
pub(crate) fn route_band_db(path: &[[f32; 3]], air_exponents_per_m: [f32; 3]) -> ([f32; 3], f32, f32) {
    let length = path
        .windows(2)
        .map(|pair| {
            ((pair[1][0] - pair[0][0]).powi(2)
                + (pair[1][1] - pair[0][1]).powi(2)
                + (pair[1][2] - pair[0][2]).powi(2))
            .sqrt()
        })
        .sum::<f32>();
    let mut turn = 0.0_f32;
    for triple in path.windows(3) {
        let a = [triple[1][0] - triple[0][0], triple[1][1] - triple[0][1]];
        let b = [triple[2][0] - triple[1][0], triple[2][1] - triple[1][1]];
        let (la, lb) = ((a[0] * a[0] + a[1] * a[1]).sqrt(), (b[0] * b[0] + b[1] * b[1]).sqrt());
        if la > 1.0e-3 && lb > 1.0e-3 {
            turn += ((a[0] * b[0] + a[1] * b[1]) / (la * lb)).clamp(-1.0, 1.0).acos();
        }
    }
    let spreading = -20.0 * length.max(1.0).log10();
    let bands = [0, 1, 2].map(|band| {
        spreading
            - 8.685_89 * air_exponents_per_m[band] * length
            - 8.685_89 * CORNER_LOSS_PER_RADIAN[band] * turn
    });
    (bands, length, turn)
}

/// One walkable dot's music: `[east, north, low, mid, high, route_m]`,
/// band levels in dB re 1 m from the speaker.
pub(crate) type FieldDot = [f32; 6];

pub(crate) fn field_json(id: &str, source: [f32; 3], dots: &[FieldDot]) -> String {
    serde_json::json!({
        "type": "field",
        "id": id,
        "source_m": source.map(|value| round(value, 10.0)),
        "dots": dots.iter().map(|dot| [
            round(dot[0], 10.0), round(dot[1], 10.0),
            dot[2].round(), dot[3].round(), dot[4].round(), dot[5].round(),
        ]).collect::<Vec<_>>(),
    })
    .to_string()
}

pub(crate) fn track_json(id: &str, track: &BandTrack) -> String {
    serde_json::json!({ "type": "track", "id": id, "track": track }).to_string()
}

/// The music's paths to You: the routed (or direct) primary and each
/// facade or corner echo, with its per-band level at You and wall point.
pub(crate) fn paths_json(
    id: &str,
    listener: [f32; 3],
    plan: &crate::echo_paths::EchoPathPlan,
    air_exponents_per_m: [f32; 3],
) -> String {
    let flat = |points: &[fightbox_api::EnuVector3]| {
        points
            .iter()
            .map(|point| [point.east_m, point.north_m, point.up_m])
            .collect::<Vec<_>>()
    };
    let primary = flat(&plan.primary_polyline_enu_m);
    let (primary_db, primary_m, _) = route_band_db(&primary, air_exponents_per_m);
    let echoes = plan
        .taps
        .iter()
        .map(|tap| {
            let points = flat(&tap.polyline_enu_m);
            let (mut band_db, length, _) = route_band_db(&points, air_exponents_per_m);
            for (level, gain) in band_db.iter_mut().zip(tap.geometry.band_pressure_gain) {
                *level += 20.0 * gain.max(1.0e-6).log10();
            }
            let wall = tap.geometry.arrival_position_enu;
            serde_json::json!({
                "points": points.iter().map(|p| p.map(|v| round(v, 10.0))).collect::<Vec<_>>(),
                "band_db": band_db.map(|v| round(v, 10.0)),
                "length_m": round(length, 10.0),
                "wall_m": [round(wall.east_m, 10.0), round(wall.north_m, 10.0), round(wall.up_m, 10.0)],
                "facade_id": tap.facade_id,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "type": "paths",
        "id": id,
        "listener_m": listener.map(|value| round(value, 10.0)),
        "line_of_sight": plan.line_of_sight,
        "primary": {
            "points": primary.iter().map(|p| p.map(|v| round(v, 10.0))).collect::<Vec<_>>(),
            "band_db": primary_db.map(|v| round(v, 10.0)),
            "length_m": round(primary_m, 10.0),
        },
        "echoes": echoes,
    })
    .to_string()
}

/// Map dots: every street-level probe at full quality, then a thinning
/// fringe off the baked area (quality `exp(-d / FALLOFF_M)`). Only where
/// you could stand and hear: never inside a footprint or on a roof (no
/// probe sits up there), and only within reach of a street, path or alley,
/// so water and closed block interiors stay blank.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct DotField {
    pub source: &'static str,
    pub spacing_m: f32,
    /// `[east_m, north_m, quality 0..1]`.
    pub points: Vec<[f32; 3]>,
    /// Marks dropped by the access rule (baked probes plus fringe).
    pub culled: usize,
}

const DOT_SPACING_M: f32 = 4.0;
/// How far from a street, path or alley centreline a mark may sit.
pub(crate) const ACCESS_M: f32 = 14.0;
const FRINGE_M: f32 = 160.0;
const FALLOFF_M: f32 = 22.0;
const MIN_QUALITY: f32 = 0.03;

pub(crate) fn quality_dots(
    probes: &[[f32; 3]],
    bounds: (Point, Point),
    roofs: &[[Point; 3]],
    access: &[Vec<Point>],
) -> DotField {
    let spacing = DOT_SPACING_M;
    let min = [bounds.0[0] - FRINGE_M, bounds.0[1] - FRINGE_M];
    let max = [bounds.1[0] + FRINGE_M, bounds.1[1] + FRINGE_M];
    let width = (((max[0] - min[0]) / spacing).ceil().max(1.0) as usize).min(2048);
    let height = (((max[1] - min[1]) / spacing).ceil().max(1.0) as usize).min(2048);
    let center = |column: usize, row: usize| {
        [
            min[0] + (column as f32 + 0.5) * spacing,
            min[1] + (row as f32 + 0.5) * spacing,
        ]
    };
    let cell = |point: [f32; 2]| {
        let column = ((point[0] - min[0]) / spacing).floor();
        let row = ((point[1] - min[1]) / spacing).floor();
        (column >= 0.0 && row >= 0.0 && (column as usize) < width && (row as usize) < height)
            .then(|| row as usize * width + column as usize)
    };

    // Each cell keeps the roof pieces over it, for exact probe tests.
    let mut over = vec![Vec::<u32>::new(); width * height];
    let mut footprint = vec![false; width * height];
    for (number, triangle) in roofs.iter().enumerate() {
        let low = cell([
            triangle.iter().map(|p| p[0]).fold(f32::INFINITY, f32::min),
            triangle.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min),
        ]);
        let high = cell([
            triangle.iter().map(|p| p[0]).fold(f32::NEG_INFINITY, f32::max),
            triangle.iter().map(|p| p[1]).fold(f32::NEG_INFINITY, f32::max),
        ]);
        let (Some(low), Some(high)) = (low, high) else {
            continue;
        };
        for row in low / width..=high / width {
            for column in low % width..=high % width {
                over[row * width + column].push(number as u32);
                if inside_triangle(center(column, row), triangle) {
                    footprint[row * width + column] = true;
                }
            }
        }
    }

    // Cells within reach of a street, path or alley (all of them when the
    // scene has no street lines).
    let mut reachable = vec![access.is_empty(); width * height];
    for line in access {
        for pair in line.windows(2) {
            let [a, b] = [pair[0], pair[1]];
            let low = [a[0].min(b[0]) - ACCESS_M, a[1].min(b[1]) - ACCESS_M];
            let high = [a[0].max(b[0]) + ACCESS_M, a[1].max(b[1]) + ACCESS_M];
            let first = [
                ((low[0] - min[0]) / spacing).floor().max(0.0) as usize,
                ((low[1] - min[1]) / spacing).floor().max(0.0) as usize,
            ];
            let last = [
                (((high[0] - min[0]) / spacing).floor().max(0.0) as usize).min(width - 1),
                (((high[1] - min[1]) / spacing).floor().max(0.0) as usize).min(height - 1),
            ];
            for row in first[1]..=last[1] {
                for column in first[0]..=last[0] {
                    if segment_distance(center(column, row), a, b) <= ACCESS_M {
                        reachable[row * width + column] = true;
                    }
                }
            }
        }
    }
    let mut culled = 0;

    let mut seed = vec![false; width * height];
    let mut points = Vec::new();
    let source = if probes.is_empty() {
        for row in 0..height {
            for column in 0..width {
                let at = center(column, row);
                let index = row * width + column;
                if !footprint[index]
                    && (bounds.0[0]..=bounds.1[0]).contains(&at[0])
                    && (bounds.0[1]..=bounds.1[1]).contains(&at[1])
                {
                    seed[index] = true;
                    if reachable[index] {
                        points.push([round(at[0], 10.0), round(at[1], 10.0), 1.0]);
                    } else {
                        culled += 1;
                    }
                }
            }
        }
        "street grid (no probe data)"
    } else {
        let mut seen = std::collections::HashSet::new();
        for probe in probes {
            let Some(index) = cell([probe[0], probe[1]]) else {
                continue;
            };
            // Roof and indoor probes sit inside footprints; dots show streets.
            let indoors = over[index]
                .iter()
                .any(|number| inside_triangle([probe[0], probe[1]], &roofs[*number as usize]));
            if indoors || !seen.insert(((probe[0] * 2.0) as i32, (probe[1] * 2.0) as i32)) {
                continue;
            }
            // The bake still counts for the fringe; the mark shows only where
            // you could stand.
            seed[index] = true;
            if reachable[index] {
                points.push([round(probe[0], 10.0), round(probe[1], 10.0), 1.0]);
            } else {
                culled += 1;
            }
        }
        "baked probes"
    };

    // Two-pass chamfer distance (metres) from the baked cells.
    let mut distance = seed
        .iter()
        .map(|seeded| if *seeded { 0.0 } else { f32::INFINITY })
        .collect::<Vec<_>>();
    let diagonal = spacing * std::f32::consts::SQRT_2;
    for row in 0..height {
        for column in 0..width {
            let index = row * width + column;
            let mut best = distance[index];
            if column > 0 {
                best = best.min(distance[index - 1] + spacing);
            }
            if row > 0 {
                best = best.min(distance[index - width] + spacing);
                if column > 0 {
                    best = best.min(distance[index - width - 1] + diagonal);
                }
                if column + 1 < width {
                    best = best.min(distance[index - width + 1] + diagonal);
                }
            }
            distance[index] = best;
        }
    }
    for row in (0..height).rev() {
        for column in (0..width).rev() {
            let index = row * width + column;
            let mut best = distance[index];
            if column + 1 < width {
                best = best.min(distance[index + 1] + spacing);
            }
            if row + 1 < height {
                best = best.min(distance[index + width] + spacing);
                if column + 1 < width {
                    best = best.min(distance[index + width + 1] + diagonal);
                }
                if column > 0 {
                    best = best.min(distance[index + width - 1] + diagonal);
                }
            }
            distance[index] = best;
        }
    }

    // Cells next to a probe are part of the baked field already; the fringe
    // starts beyond them and thins with distance.
    let edge = 1.5 * spacing;
    for row in 0..height {
        for column in 0..width {
            let index = row * width + column;
            if footprint[index] || distance[index] <= edge {
                continue;
            }
            let quality = (-(distance[index] - edge) / FALLOFF_M).exp();
            if quality < MIN_QUALITY || unit_hash(column, row) >= quality {
                continue;
            }
            if !reachable[index] {
                culled += 1;
                continue;
            }
            let at = center(column, row);
            points.push([round(at[0], 10.0), round(at[1], 10.0), round(quality, 100.0)]);
        }
    }
    DotField {
        source,
        spacing_m: spacing,
        points,
        culled,
    }
}

fn round(value: f32, scale: f32) -> f32 {
    (value * scale).round() / scale
}

pub(crate) fn segment_distance(point: Point, a: Point, b: Point) -> f32 {
    let along = [b[0] - a[0], b[1] - a[1]];
    let length = along[0] * along[0] + along[1] * along[1];
    let t = if length > 0.0 {
        (((point[0] - a[0]) * along[0] + (point[1] - a[1]) * along[1]) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let nearest = [a[0] + along[0] * t, a[1] + along[1] * t];
    ((point[0] - nearest[0]).powi(2) + (point[1] - nearest[1]).powi(2)).sqrt()
}

fn inside_triangle(point: Point, [a, b, c]: &[Point; 3]) -> bool {
    let side = |p: Point, q: Point| (q[0] - p[0]) * (point[1] - p[1]) - (q[1] - p[1]) * (point[0] - p[0]);
    let (ab, bc, ca) = (side(*a, *b), side(*b, *c), side(*c, *a));
    (ab >= 0.0 && bc >= 0.0 && ca >= 0.0) || (ab <= 0.0 && bc <= 0.0 && ca <= 0.0)
}

/// Stable per-cell value in [0, 1), so the thinning never shimmers.
fn unit_hash(column: usize, row: usize) -> f32 {
    let mut value = (column as u64) << 32 | row as u64;
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^= value >> 33;
    (value >> 40) as f32 / (1_u64 << 24) as f32
}

/// The scene's geographic frame and named landmarks, read from the
/// `city-build.json` and source geojson beside a city package. Display
/// metadata only; acoustic geometry never reads it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SceneGeo {
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub landmarks: Vec<Landmark>,
    /// Famous towers in the scene, at their real roof height.
    pub towers: Vec<Tower>,
    /// Footprints and heights, for the map's occlusion at a tilt.
    pub buildings: Vec<Building>,
}

/// One footprint for drawing: outer ring and roof height (famous towers at
/// their real roof, as the map's own 3D buildings show them).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Building {
    pub ring: Vec<Point>,
    pub height_m: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Landmark {
    pub name: String,
    pub at: Point,
}

/// A landmark tower whose real roof the scene's footprint median misses
/// (Willis Tower's lot is mostly podium and plaza, so the build gives it
/// ~20 m). The spot uses the real roof; acoustics still use the scene.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tower {
    /// What md calls it, e.g. "Sears Tower".
    pub label: &'static str,
    pub key: &'static str,
    pub at: Point,
    pub roof_m: f32,
}

/// (names in the map data, label, key, roof height in metres). Roof, not
/// antenna tips.
const FAMOUS_TOWERS: [(&[&str], &str, &str, f32); 1] =
    [(&["Willis Tower", "Sears Tower"], "Sears Tower", "sears-tower", 442.1)];

/// Spots on famous towers play "really, really loud": the sound's level
/// trim rises toward this SPL at 1 m, within the Workbench's own trim range
/// and before the untouched output limiter.
pub(crate) const TOWER_LOUD_SPL_DB: f32 = 139.0;

const EARTH_RADIUS_M: f64 = 6_371_008.8;

impl SceneGeo {
    pub(crate) fn read(package: &Path) -> Option<Self> {
        let scene = package.parent()?;
        let build: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scene.join("city-build.json")).ok()?).ok()?;
        let origin = build.get("origin")?;
        let mut geo = Self {
            latitude_deg: origin.get("latitude_degrees")?.as_f64()?,
            longitude_deg: origin.get("longitude_degrees")?.as_f64()?,
            landmarks: Vec::new(),
            towers: Vec::new(),
            buildings: Vec::new(),
        };
        if let Some(features) = std::fs::read(scene.join("city.geojson"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        {
            geo.landmarks = geo.worship_buildings(&features);
            geo.towers = geo.famous_towers(&features);
            geo.buildings = geo.footprints(&features);
        }
        Some(geo)
    }

    /// The city build's local equirectangular frame (`fn fixture` origin).
    pub(crate) fn to_enu(&self, latitude_deg: f64, longitude_deg: f64) -> Point {
        let east = EARTH_RADIUS_M
            * self.latitude_deg.to_radians().cos()
            * (longitude_deg - self.longitude_deg).to_radians();
        let north = EARTH_RADIUS_M * (latitude_deg - self.latitude_deg).to_radians();
        [east as f32, north as f32]
    }

    fn worship_buildings(&self, geojson: &serde_json::Value) -> Vec<Landmark> {
        const WORSHIP: [&str; 7] = [
            "church", "cathedral", "chapel", "temple", "mosque", "synagogue", "shrine",
        ];
        let Some(features) = geojson.get("features").and_then(|value| value.as_array()) else {
            return Vec::new();
        };
        features
            .iter()
            .filter_map(|feature| {
                let properties = feature.get("properties")?;
                let name = properties.get("name")?.as_str()?.trim();
                let building = properties.get("building").and_then(|v| v.as_str());
                let amenity = properties.get("amenity").and_then(|v| v.as_str());
                if name.is_empty()
                    || !(building.is_some_and(|kind| WORSHIP.contains(&kind))
                        || amenity == Some("place_of_worship"))
                {
                    return None;
                }
                Some(Landmark {
                    name: name.to_owned(),
                    at: self.footprint_centre(feature)?,
                })
            })
            .collect()
    }

    fn famous_towers(&self, geojson: &serde_json::Value) -> Vec<Tower> {
        let Some(features) = geojson.get("features").and_then(|value| value.as_array()) else {
            return Vec::new();
        };
        FAMOUS_TOWERS
            .iter()
            .filter_map(|(names, label, key, roof_m)| {
                let feature = features.iter().find(|feature| {
                    feature
                        .get("properties")
                        .and_then(|properties| properties.get("name"))
                        .and_then(|name| name.as_str())
                        .is_some_and(|name| names.contains(&name.trim()))
                })?;
                Some(Tower {
                    label,
                    key,
                    at: self.footprint_centre(feature)?,
                    roof_m: *roof_m,
                })
            })
            .collect()
    }

    fn footprints(&self, geojson: &serde_json::Value) -> Vec<Building> {
        let Some(features) = geojson.get("features").and_then(|value| value.as_array()) else {
            return Vec::new();
        };
        features
            .iter()
            .filter_map(|feature| {
                let properties = feature.get("properties")?;
                properties.get("building")?;
                let name = properties.get("name").and_then(|name| name.as_str()).unwrap_or("");
                let height_m = FAMOUS_TOWERS
                    .iter()
                    .find(|(names, ..)| names.contains(&name.trim()))
                    .map(|(.., roof_m)| *roof_m)
                    .or_else(|| properties.get("height").and_then(|h| h.as_f64()).map(|h| h as f32))
                    .filter(|height| height.is_finite() && *height > 0.0)
                    .unwrap_or(8.0);
                let ring = self
                    .footprint_ring(feature)?
                    .into_iter()
                    .map(|point| [round(point[0], 10.0), round(point[1], 10.0)])
                    .collect::<Vec<_>>();
                (ring.len() >= 3).then_some(Building {
                    ring,
                    height_m: round(height_m, 10.0),
                })
            })
            .collect()
    }

    fn footprint_ring(&self, feature: &serde_json::Value) -> Option<Vec<Point>> {
        let geometry = feature.get("geometry")?;
        let rings = geometry.get("coordinates")?.as_array()?;
        let ring = match geometry.get("type")?.as_str()? {
            "Polygon" => rings.first()?,
            "MultiPolygon" => rings.first()?.as_array()?.first()?,
            _ => return None,
        }
        .as_array()?;
        Some(
            ring.iter()
                .filter_map(|pair| {
                    let pair = pair.as_array()?;
                    Some(self.to_enu(pair.get(1)?.as_f64()?, pair.first()?.as_f64()?))
                })
                .collect(),
        )
    }

    /// Area centroid of a feature's outer ring, so dense vertex runs along
    /// one side do not pull the spot off the building.
    fn footprint_centre(&self, feature: &serde_json::Value) -> Option<Point> {
        let geometry = feature.get("geometry")?;
        let rings = geometry.get("coordinates")?.as_array()?;
        let ring = match geometry.get("type")?.as_str()? {
            "Polygon" => rings.first()?,
            "MultiPolygon" => rings.first()?.as_array()?.first()?,
            _ => return None,
        }
        .as_array()?;
        let points = ring
            .iter()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some(self.to_enu(pair.get(1)?.as_f64()?, pair.first()?.as_f64()?))
            })
            .collect::<Vec<_>>();
        area_centroid(&points)
    }
}

/// Polygon area centroid; falls back to the vertex mean for degenerate rings.
fn area_centroid(points: &[Point]) -> Option<Point> {
    // Closed rings repeat the first vertex.
    let unique = match points {
        [] => return None,
        [first, .., last] if first == last && points.len() > 1 => &points[..points.len() - 1],
        all => all,
    };
    let origin = unique[0];
    let (mut area, mut east, mut north) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (index, a) in unique.iter().enumerate() {
        let b = unique[(index + 1) % unique.len()];
        let (ax, ay) = (f64::from(a[0] - origin[0]), f64::from(a[1] - origin[1]));
        let (bx, by) = (f64::from(b[0] - origin[0]), f64::from(b[1] - origin[1]));
        let cross = ax * by - bx * ay;
        area += cross;
        east += (ax + bx) * cross;
        north += (ay + by) * cross;
    }
    if area.abs() < 1e-6 {
        let count = unique.len() as f32;
        return Some(unique.iter().fold([0.0, 0.0], |sum, point| {
            [sum[0] + point[0] / count, sum[1] + point[1] / count]
        }));
    }
    Some([
        origin[0] + (east / (3.0 * area)) as f32,
        origin[1] + (north / (3.0 * area)) as f32,
    ])
}

/// A suggested place for the selected speaker, height included.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Spot {
    pub key: String,
    pub label: String,
    pub detail: String,
    pub east_m: f32,
    pub north_m: f32,
    pub above_top_m: f32,
    /// Absolute height, for drawing.
    pub up_m: f32,
    /// The map uses your live position instead of `east_m`/`north_m`.
    pub follows_listener: bool,
    /// Picking this spot raises the sound's level trim toward this SPL at
    /// 1 m; moving it anywhere else puts the trim back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loud_spl_db: Option<f32>,
}

pub(crate) struct SpotInputs<'a> {
    pub listener: Point,
    pub bounds: (Point, Point),
    /// Flat roof pieces: centroid and height.
    pub roofs: &'a [(Point, f32)],
    /// Street-level baked points (map dots at full quality).
    pub street: &'a [Point],
    pub landmarks: &'a [Landmark],
    pub towers: &'a [Tower],
    /// Named street crossings, e.g. "N Main St & W First Ave".
    pub corners: &'a [(Point, String)],
    pub top: &'a dyn Fn(Point) -> f32,
    pub covered: &'a dyn Fn([f32; 3]) -> bool,
    pub street_at: &'a dyn Fn(Point) -> String,
}

/// Places worth trying in this scene, every one already checked against the
/// bake: famous tower tops, tallest roof, worship towers, the far map
/// corner, a street corner, and straight overhead.
pub(crate) fn suggest_spots(inputs: &SpotInputs<'_>) -> Vec<Spot> {
    let top = inputs.top;
    let nearest_streets = |at: Point, within: f32| {
        let mut near = inputs
            .street
            .iter()
            .copied()
            .map(|point| (point, crate::walk_view::distance(point, at)))
            .filter(|(_, metres)| *metres <= within)
            .collect::<Vec<_>>();
        near.sort_by(|a, b| a.1.total_cmp(&b.1));
        near.into_iter().map(|(point, _)| point).take(16)
    };
    // On the spot when it is baked; otherwise the nearest baked street at
    // the same absolute height.
    let anchor_within = |at: Point, above: f32, within: f32| -> Option<(Point, f32, bool)> {
        let surface = top(at);
        if landing(at, surface, above, inputs.covered).is_some() {
            return Some((at, above, false));
        }
        nearest_streets(at, within).find_map(|street| {
            let above_street = surface + above - top(street);
            landing(street, top(street), above_street, inputs.covered)
                .map(|_| (street, above_street, true))
        })
    };
    let anchor = |at: Point, above: f32| anchor_within(at, above, 40.0);
    let spot = |key: String, label: String, detail: String, at: Point, above: f32| Spot {
        key,
        label,
        detail,
        east_m: round(at[0], 10.0),
        north_m: round(at[1], 10.0),
        above_top_m: round(above, 10.0),
        up_m: round(top(at) + above, 10.0),
        follows_listener: false,
        loud_spl_db: None,
    };
    let mut spots = Vec::new();

    for tower in inputs.towers {
        // 3 m over the real roof. A big lot can sit far from any street.
        let above = tower.roof_m + 3.0 - top(tower.at);
        if let Some((at, above, beside)) = anchor_within(tower.at, above, 90.0) {
            let mut loud = spot(
                tower.key.into(),
                format!("Top of the {}", tower.label),
                format!(
                    "{} m up{} · really, really loud",
                    (top(at) + above).round(),
                    if beside { ", street side" } else { "" }
                ),
                at,
                above,
            );
            loud.loud_spl_db = Some(TOWER_LOUD_SPL_DB);
            spots.push(loud);
        }
    }

    let mut roofs = inputs.roofs.to_vec();
    roofs.sort_by(|a, b| b.1.total_cmp(&a.1));
    if let Some((at, above, beside)) = roofs.iter().take(64).find_map(|(at, _)| anchor(*at, 3.0)) {
        let up = top(at) + above;
        spots.push(spot(
            "tallest-roof".into(),
            "Tallest roof".into(),
            format!(
                "{} m up{} · over the whole block",
                up.round(),
                if beside { ", street side" } else { "" }
            ),
            at,
            above,
        ));
    }

    let mut landmarks = inputs.landmarks.iter().collect::<Vec<_>>();
    landmarks.sort_by(|a, b| {
        crate::walk_view::distance(a.at, inputs.listener)
            .total_cmp(&crate::walk_view::distance(b.at, inputs.listener))
    });
    for (index, landmark) in landmarks.iter().take(3).enumerate() {
        // A bell tower stands about 8 m over the nave roof.
        if let Some((at, above, beside)) = anchor(landmark.at, 8.0) {
            spots.push(spot(
                format!("tower-{index}"),
                format!("Bells · {}", short_landmark(&landmark.name)),
                format!(
                    "in the tower, {} m up{}",
                    (top(at) + above).round(),
                    if beside { ", street side" } else { "" }
                ),
                at,
                above,
            ));
        }
    }

    let (low, high) = inputs.bounds;
    let far_corner = [[low[0], low[1]], [high[0], low[1]], [low[0], high[1]], [high[0], high[1]]]
        .into_iter()
        .max_by(|a, b| {
            crate::walk_view::distance(*a, inputs.listener)
                .total_cmp(&crate::walk_view::distance(*b, inputs.listener))
        });
    if let Some(at) = far_corner.and_then(|corner| nearest_streets(corner, f32::INFINITY).next()) {
        spots.push(spot(
            "map-corner".into(),
            "Map corner".into(),
            format!(
                "{} · {} away · artillery range",
                (inputs.street_at)(at),
                crate::walk_view::format_distance(crate::walk_view::distance(at, inputs.listener))
            ),
            at,
            1.5,
        ));
    }

    if let Some((at, name)) = inputs
        .corners
        .iter()
        .filter(|(at, _)| crate::walk_view::distance(*at, inputs.listener) >= 25.0)
        .min_by(|a, b| {
            crate::walk_view::distance(a.0, inputs.listener)
                .total_cmp(&crate::walk_view::distance(b.0, inputs.listener))
        })
        && let Some((at, above, _)) = anchor(*at, 1.5)
    {
        spots.push(spot(
            "street-corner".into(),
            "Street corner".into(),
            name.clone(),
            at,
            above,
        ));
    }

    let mut overhead = spot(
        "overhead".into(),
        "Overhead".into(),
        "80 m above you · helicopter height".into(),
        inputs.listener,
        80.0,
    );
    overhead.follows_listener = true;
    spots.push(overhead);
    spots
}

fn short_landmark(name: &str) -> String {
    let short = name
        .trim_end_matches(" Church")
        .trim_end_matches(" Roman Catholic")
        .trim_end_matches(" Episcopal")
        .trim_end_matches(" United Methodist");
    if short.len() <= 26 {
        short.to_owned()
    } else {
        format!("{}…", short.chars().take(25).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: [&str; 3] = ["music", "bells", "helicopter"];
    const SPOTS: [&str; 2] = ["tallest-roof", "sears-tower"];

    #[test]
    fn parses_every_request_the_map_sends() {
        // These exact lines are what CityMap's LinkCommand encodes.
        let cases = [
            (
                r#"{"type":"move","id":"music","east_m":12.5,"north_m":-3,"done":false}"#,
                MapAction::Move {
                    index: 0,
                    at: [12.5, -3.0],
                    above_top_m: None,
                    done: false,
                },
            ),
            (
                r#"{"type":"move","id":"bells","east_m":1,"north_m":2,"above_top_m":8,"done":true}"#,
                MapAction::Move {
                    index: 1,
                    at: [1.0, 2.0],
                    above_top_m: Some(8.0),
                    done: true,
                },
            ),
            (
                r#"{"type":"set_on","id":"helicopter","on":true}"#,
                MapAction::SetOn { index: 2, on: true },
            ),
            (
                r#"{"type":"spot","id":"music","key":"sears-tower"}"#,
                MapAction::Spot { index: 0, spot: 1 },
            ),
            (r#"{"type":"fire","id":"helicopter"}"#, MapAction::Fire { index: 2 }),
            (
                r#"{"type":"move_you","east_m":4,"north_m":-2.5}"#,
                MapAction::MoveYou { at: [4.0, -2.5] },
            ),
            (r#"{"type":"select","id":"bells"}"#, MapAction::Select { index: 1 }),
            (r#"{"type":"play_all"}"#, MapAction::PlayAll),
            (r#"{"type":"stop_all"}"#, MapAction::StopAll),
            (r#"{"type":"set_volume","db":12}"#, MapAction::SetVolume { db: 12.0 }),
        ];
        for (line, expected) in cases {
            let action = MapCommand::parse(line)
                .and_then(|command| command.resolve(&IDS, &SPOTS, -20.0..=40.0))
                .unwrap_or_else(|error| panic!("{line}: {error}"));
            assert_eq!(action, expected, "{line}");
        }
    }

    #[test]
    fn refuses_or_clamps_out_of_range_requests() {
        let resolve =
            |line: &str| MapCommand::parse(line).and_then(|c| c.resolve(&IDS, &SPOTS, -20.0..=40.0));
        assert!(resolve(r#"{"type":"set_on","id":"nope","on":true}"#).is_err());
        assert!(resolve(r#"{"type":"spot","id":"music","key":"moon"}"#).is_err());
        assert!(resolve(r#"{"type":"spot","id":"nope","key":"sears-tower"}"#).is_err());
        assert!(resolve(r#"{"type":"move","id":"music","east_m":1e9,"north_m":0}"#).is_err());
        assert!(resolve(r#"{"type":"move","id":"music","east_m":0,"north_m":0,"above_top_m":-1}"#).is_err());
        assert!(resolve(r#"{"type":"teleport"}"#).is_err());
        assert!(resolve("not json").is_err());
        // The map can never push volume past the Workbench's own slider range.
        assert_eq!(
            resolve(r#"{"type":"set_volume","db":90}"#),
            Ok(MapAction::SetVolume { db: 40.0 })
        );
        assert_eq!(
            resolve(r#"{"type":"set_volume","db":-90}"#),
            Ok(MapAction::SetVolume { db: -20.0 })
        );
    }

    #[test]
    fn address_is_loopback_only() {
        assert_eq!(parse_address("47820").unwrap(), "127.0.0.1:47820".parse().unwrap());
        assert_eq!(parse_address("[::1]:9").unwrap(), "[::1]:9".parse().unwrap());
        assert!(parse_address("0.0.0.0:47820").is_err());
        assert!(parse_address("192.168.1.2:47820").is_err());
        assert!(parse_address("map").is_err());
        assert!(MapLink::bind("0.0.0.0:0".parse().unwrap()).is_err());
    }

    #[test]
    fn landing_checks_the_surface_and_frees_the_height() {
        // Baked only up to 4 m: the roof at 20 m is not.
        let covered = |p: [f32; 3]| p[2] <= 4.0;
        assert_eq!(landing([1.0, 2.0], 0.0, 80.0, covered), Some([1.0, 2.0, 80.0]));
        assert_eq!(landing([1.0, 2.0], 0.0, 3.0, covered), Some([1.0, 2.0, 3.0]));
        assert_eq!(landing([1.0, 2.0], 20.0, 3.0, covered), None);
    }

    #[test]
    fn dots_are_full_on_the_bake_and_thin_off_it() {
        // A 40 m baked street strip with one building beside it.
        let probes = (0..10)
            .flat_map(|i| (0..3).map(move |j| [i as f32 * 4.0, j as f32 * 4.0, 1.5]))
            .collect::<Vec<_>>();
        let roof = [[0.0, 20.0], [40.0, 20.0], [40.0, 60.0]];
        let field = quality_dots(&probes, ([0.0, 0.0], [40.0, 60.0]), &[roof], &[]);
        assert_eq!(field.source, "baked probes");
        let full = field.points.iter().filter(|p| p[2] == 1.0).count();
        assert_eq!(full, probes.len(), "every street probe is a full dot");
        let fringe = field.points.iter().filter(|p| p[2] < 1.0).collect::<Vec<_>>();
        assert!(!fringe.is_empty());
        assert!(fringe.iter().all(|p| p[2] >= MIN_QUALITY));
        // Thinning: fewer dots per ring further from the bake.
        let count = |from: f32, to: f32| {
            fringe
                .iter()
                .filter(|p| {
                    let d = (p[1] - 8.0).max(-p[1]).max(0.0).max((p[0] - 36.0).max(-p[0]));
                    (from..to).contains(&d)
                })
                .count()
        };
        assert!(count(6.0, 30.0) > count(30.0, 54.0));
        assert!(count(30.0, 54.0) > count(54.0, 120.0) / 3);
        // No fringe dot inside the building footprint.
        assert!(!fringe.iter().any(|p| inside_triangle([p[0], p[1]], &roof)));
    }

    #[test]
    fn marks_inside_a_mapped_footprint_are_dropped() {
        let mut field = DotField {
            source: "baked probes",
            spacing_m: 4.0,
            points: vec![[5.0, 5.0, 1.0], [50.0, 5.0, 1.0], [-3.0, 5.0, 0.5], [64.5, 64.5, 1.0]],
            culled: 2,
        };
        let block = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let far = vec![[60.0, 60.0], [70.0, 60.0], [70.0, 70.0], [60.0, 70.0]];
        assert_eq!(drop_inside_footprints(&mut field, &[block, far]), 2);
        assert_eq!(field.points, vec![[50.0, 5.0, 1.0], [-3.0, 5.0, 0.5]]);
        assert_eq!(field.culled, 4);
    }

    #[test]
    fn a_tap_lands_you_on_the_nearest_street_never_inside() {
        // One east-west street along y = 0 and a block north of it.
        let streets = vec![vec![[-50.0, 0.0], [50.0, 0.0]]];
        let block = vec![[-20.0, 10.0], [20.0, 10.0], [20.0, 40.0], [-20.0, 40.0]];
        let roofs = [[[30.0, 10.0], [45.0, 10.0], [45.0, 25.0]]];
        assert_eq!(you_landing([12.3, 6.0], &streets, &roofs, &[block.clone()]), Ok([12.3, 0.0]));
        assert_eq!(you_landing([60.0, -3.0], &streets, &roofs, &[block.clone()]), Ok([50.0, 0.0]));
        let refused = you_landing([0.0, 20.0], &streets, &roofs, &[block.clone()]).unwrap_err();
        assert!(refused.contains("inside a building"), "{refused}");
        assert!(you_landing([40.0, 12.0], &streets, &roofs, &[]).unwrap_err().contains("inside"));
        assert!(you_landing([0.0, -80.0], &streets, &roofs, &[block]).unwrap_err().contains("Tap a street"));
        assert!(
            MapCommand::parse(r#"{"type":"move_you","east_m":1e9,"north_m":0}"#)
                .unwrap()
                .resolve(&IDS, &SPOTS, -20.0..=40.0)
                .is_err()
        );
    }

    #[test]
    fn a_song_track_follows_its_bands_and_finds_its_kicks() {
        // Two seconds at 48 kHz: a 60 Hz kick every half second over a
        // steady 5 kHz hiss.
        let rate = 48_000;
        let samples = (0..2 * rate)
            .map(|index| {
                let time = index as f32 / rate as f32;
                let since = time % 0.5;
                let kick = (-since * 30.0).exp() * (std::f32::consts::TAU * 60.0 * time).sin();
                kick * 0.8 + 0.05 * (std::f32::consts::TAU * 5000.0 * time).sin()
            })
            .collect::<Vec<_>>();
        let track = band_track(&samples, rate as u32);
        assert_eq!(track.rate_hz, TRACK_RATE_HZ);
        assert_eq!(track.bands_db.len(), 60 * 3);
        assert!((track.length_s - 2.0).abs() < 1.0e-3);
        // Right after a kick, the low band leads; the high band holds steady.
        let frame = |second: f32| &track.bands_db[(second * 30.0) as usize * 3..][..3];
        assert!(frame(0.52)[0] > frame(0.95)[0] + 10, "{:?} {:?}", frame(0.52), frame(0.95));
        assert!((i32::from(frame(0.52)[2]) - i32::from(frame(0.95)[2])).abs() <= 2);
        assert_eq!(track.kicks_s.len(), 3, "{:?}", track.kicks_s);
        for (kick, expected) in track.kicks_s.iter().zip([0.5, 1.0, 1.5]) {
            assert!((kick - expected).abs() <= 0.04, "{:?}", track.kicks_s);
        }
    }

    #[test]
    fn routed_music_loses_highs_at_corners_and_keeps_bass() {
        let air = [0.0005, 0.002, 0.01];
        let (straight, length, turn) = route_band_db(&[[0.0, 0.0, 3.0], [100.0, 0.0, 3.0]], air);
        assert!((length - 100.0).abs() < 1.0e-3 && turn == 0.0);
        assert!((straight[0] + 40.0).abs() < 0.5, "{straight:?}");
        let (around, length, turn) =
            route_band_db(&[[0.0, 0.0, 3.0], [50.0, 0.0, 3.0], [50.0, 50.0, 3.0]], air);
        assert!((length - 100.0).abs() < 1.0e-3);
        assert!((turn - std::f32::consts::FRAC_PI_2).abs() < 1.0e-3);
        let lost = [0, 1, 2].map(|band| straight[band] - around[band]);
        assert!(lost[0] < 3.5 && lost[2] > 20.0 && lost[0] < lost[1] && lost[1] < lost[2], "{lost:?}");
    }

    #[test]
    fn dots_only_where_you_could_stand() {
        // Probes everywhere on a 100 m square; one street along y = 0.
        let probes = (0..25)
            .flat_map(|i| (0..25).map(move |j| [i as f32 * 4.0, j as f32 * 4.0, 1.5]))
            .collect::<Vec<_>>();
        let street = vec![vec![[0.0, 0.0], [100.0, 0.0]]];
        let field = quality_dots(&probes, ([0.0, 0.0], [100.0, 100.0]), &[], &street);
        assert!(field.culled > 0);
        // Nothing beyond reach of the street (cell centres can sit up to
        // half a diagonal off the probe).
        let slack = DOT_SPACING_M * std::f32::consts::FRAC_1_SQRT_2;
        assert!(field.points.iter().all(|p| p[1] <= ACCESS_M + slack), "{:?}", field.points.iter().map(|p| p[1]).fold(0.0, f32::max));
        assert!(field.points.iter().filter(|p| p[2] == 1.0).count() >= 25 * 3);
        // The fringe still thins from the whole bake, but only along the street.
        assert!(field.points.iter().any(|p| p[2] < 1.0 && p[0] > 100.0));
    }

    #[test]
    fn dots_fall_back_to_a_street_grid_without_probes() {
        let roof = [[0.0, 0.0], [20.0, 0.0], [0.0, 20.0]];
        let field = quality_dots(&[], ([0.0, 0.0], [40.0, 40.0]), &[roof], &[]);
        assert_eq!(field.source, "street grid (no probe data)");
        let full = field.points.iter().filter(|p| p[2] == 1.0).collect::<Vec<_>>();
        assert!(!full.is_empty());
        assert!(!full.iter().any(|p| inside_triangle([p[0], p[1]], &roof)));
    }

    #[test]
    fn spots_are_checked_against_the_bake() {
        let roofs = [([50.0, 50.0], 30.0), ([10.0, 10.0], 12.0)];
        let street = [[0.0, 0.0], [48.0, 40.0], [100.0, 100.0], [-90.0, -90.0]];
        let landmarks = [Landmark {
            name: "Saint Example Church".into(),
            at: [12.0, 10.0],
        }];
        let corners = [([40.0, 0.0], "A St & B Ave".to_owned())];
        // A famous tower on a low scene lot (10 m), far from the listener.
        let towers = [Tower {
            label: "Sears Tower",
            key: "sears-tower",
            at: [-60.0, 60.0],
            roof_m: 442.1,
        }];
        // Roofs over 20 m are unbaked; streets and the low roof are baked.
        let top = |at: Point| {
            if crate::walk_view::distance(at, [-60.0, 60.0]) < 5.0 {
                return 10.0;
            }
            roofs
                .iter()
                .find(|(roof, _)| crate::walk_view::distance(*roof, at) < 5.0)
                .map_or(0.0, |(_, height)| *height)
        };
        // Streets and the low church roof are baked; the tower lot is not.
        let covered = |p: [f32; 3]| p[2] <= 1.5 || (p[2] <= 20.0 && p[0] > 0.0);
        let spots = suggest_spots(&SpotInputs {
            listener: [0.0, 0.0],
            bounds: ([-100.0, -100.0], [100.0, 100.0]),
            roofs: &roofs,
            street: &street,
            landmarks: &landmarks,
            towers: &towers,
            corners: &corners,
            top: &top,
            covered: &covered,
            street_at: &|_| "A St".to_owned(),
        });
        let keys = spots.iter().map(|spot| spot.key.as_str()).collect::<Vec<_>>();
        assert_eq!(
            keys,
            ["sears-tower", "tallest-roof", "tower-0", "map-corner", "street-corner", "overhead"]
        );
        // The tower lot is unbaked, so the speaker goes up from the nearest
        // baked street at the real roof height plus 3 m, and plays loud.
        let sears = &spots[0];
        assert_eq!(sears.label, "Top of the Sears Tower");
        assert_eq!([sears.east_m, sears.north_m], [0.0, 0.0]);
        assert!((sears.up_m - 445.1).abs() < 0.11, "{sears:?}");
        assert!(sears.detail.contains("really, really loud") && sears.detail.contains("street side"));
        assert_eq!(sears.loud_spl_db, Some(TOWER_LOUD_SPL_DB));
        assert!(spots[1..].iter().all(|spot| spot.loud_spl_db.is_none()));
        // The 30 m roof is unbaked, so the PA goes beside it at the same height.
        assert_eq!([spots[1].east_m, spots[1].north_m], [48.0, 40.0]);
        assert_eq!(spots[1].up_m, 33.0);
        assert!(spots[1].detail.contains("street side"));
        // The church roof (12 m) is baked: the bells sit 8 m over it.
        assert_eq!(spots[2].label, "Bells · Saint Example");
        assert_eq!(spots[2].up_m, 20.0);
        assert_eq!([spots[3].east_m, spots[3].north_m], [100.0, 100.0]);
        assert!(spots[5].follows_listener);
        for spot in &spots[..5] {
            assert!(
                landing(
                    [spot.east_m, spot.north_m],
                    top([spot.east_m, spot.north_m]),
                    spot.above_top_m,
                    covered
                )
                .is_some(),
                "{} must land",
                spot.key
            );
        }
    }

    #[test]
    fn scene_geo_matches_the_city_build_projection() {
        let geo = SceneGeo {
            latitude_deg: 41.9656215,
            longitude_deg: -87.6729939,
            landmarks: Vec::new(),
            towers: Vec::new(),
            buildings: Vec::new(),
        };
        let east = geo.to_enu(41.9656215, -87.6729939 + 0.001);
        let north = geo.to_enu(41.9656215 + 0.001, -87.6729939);
        // 0.001 degree is ~82.7 m east and ~111.2 m north at this latitude.
        assert!((east[0] - 82.72).abs() < 0.05 && east[1].abs() < 1e-3, "{east:?}");
        assert!((north[1] - 111.19).abs() < 0.05 && north[0].abs() < 1e-3, "{north:?}");
        let geojson = serde_json::json!({"features": [
            {"properties": {"building": "church", "name": "Test Church"},
             "geometry": {"type": "Polygon", "coordinates": [[
                [-87.6729939, 41.9656215], [-87.6719939, 41.9656215],
                [-87.6719939, 41.9666215], [-87.6729939, 41.9666215],
                [-87.6729939, 41.9656215]]]}},
            {"properties": {"building": "yes", "name": "Not A Church"},
             "geometry": {"type": "Polygon", "coordinates": [[[0, 0], [1, 0], [0, 1], [0, 0]]]}}
        ]});
        let landmarks = geo.worship_buildings(&geojson);
        assert_eq!(landmarks.len(), 1);
        assert!((landmarks[0].at[0] - 41.36).abs() < 0.05, "{:?}", landmarks[0].at);
        assert!((landmarks[0].at[1] - 55.6).abs() < 0.05, "{:?}", landmarks[0].at);
        assert!(geo.famous_towers(&geojson).is_empty());

        let willis = serde_json::json!({"features": [
            {"properties": {"building": "commercial", "name": "Willis Tower", "height": 19.5},
             "geometry": {"type": "Polygon", "coordinates": [[
                [-87.6729939, 41.9656215], [-87.6719939, 41.9656215],
                [-87.6719939, 41.9666215], [-87.6729939, 41.9666215],
                [-87.6729939, 41.9656215]]]}}
        ]});
        let towers = geo.famous_towers(&willis);
        let buildings = geo.footprints(&willis);
        assert_eq!(buildings.len(), 1);
        assert_eq!(buildings[0].height_m, 442.1, "the map shows Willis at its real roof");
        assert_eq!(buildings[0].ring.len(), 5);
        assert_eq!(towers.len(), 1);
        assert_eq!((towers[0].label, towers[0].key), ("Sears Tower", "sears-tower"));
        assert_eq!(towers[0].roof_m, 442.1);
        assert!((towers[0].at[0] - 41.36).abs() < 0.05 && (towers[0].at[1] - 55.6).abs() < 0.05);
    }

    #[test]
    fn footprint_centre_is_the_area_centroid() {
        // An L: a dense vertex run along one edge must not pull the centre.
        let mut ring = vec![[0.0, 0.0], [10.0, 0.0]];
        ring.extend((1..20).map(|step| [10.0, step as f32 * 0.5]));
        ring.extend([[10.0, 10.0], [0.0, 10.0], [0.0, 0.0]]);
        let centre = area_centroid(&ring).unwrap();
        assert!((centre[0] - 5.0).abs() < 1e-3 && (centre[1] - 5.0).abs() < 1e-3, "{centre:?}");
        assert_eq!(area_centroid(&[[1.0, 1.0]]), Some([1.0, 1.0]));
        assert_eq!(area_centroid(&[]), None);
    }

    #[test]
    fn socket_round_trip_greets_reads_and_broadcasts() {
        use std::io::{BufRead, BufReader};
        let mut link = MapLink::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let client = TcpStream::connect(link.address).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !link.accept() {
            assert!(std::time::Instant::now() < deadline, "client never accepted");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        link.greet(r#"{"type":"hello","protocol":1}"#);
        assert!(!link.accept(), "greeted clients need no second hello");
        link.flush();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), r#"{"type":"hello","protocol":1}"#);

        (&client)
            .write_all(b"{\"type\":\"play_all\"}\n{\"type\":\"set_volume\",\"db\":")
            .unwrap();
        let mut commands = Vec::new();
        while commands.is_empty() {
            assert!(std::time::Instant::now() < deadline + std::time::Duration::from_secs(1));
            commands = link.read_commands();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(commands, [Ok(MapCommand::PlayAll)]);
        // The half-written line completes on a later read.
        (&client).write_all(b"3}\n").unwrap();
        let mut commands = Vec::new();
        while commands.is_empty() {
            commands = link.read_commands();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(commands, [Ok(MapCommand::SetVolume { db: 3.0 })]);

        link.notice("No baked path there");
        link.broadcast(r#"{"type":"state"}"#);
        link.flush();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            serde_json::json!({"type": "notice", "text": "No baked path there"})
        );
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), r#"{"type":"state"}"#);

        drop(reader);
        drop(client);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while link.has_clients() {
            assert!(std::time::Instant::now() < deadline, "closed client never dropped");
            link.read_commands();
            link.flush();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
