//! Predicted audible arrivals, in local ENU and seconds from the audio trigger.

use fightbox_api::EnuVector3;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const SPEED_OF_SOUND_MPS: f64 = 343.0;

mod enu {
    use super::*;
    pub fn values(point: EnuVector3) -> [f32; 3] {
        [point.east_m, point.north_m, point.up_m]
    }
    pub fn point([east, north, up]: [f32; 3]) -> EnuVector3 {
        EnuVector3::new(east, north, up)
    }
    pub fn serialize<S: Serializer>(point: &EnuVector3, serializer: S) -> Result<S::Ok, S::Error> {
        values(*point).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<EnuVector3, D::Error> {
        Ok(point(<[f32; 3]>::deserialize(deserializer)?))
    }
}

mod polyline {
    use super::*;
    pub fn serialize<S: Serializer>(
        path: &List<EnuVector3, 64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(path.iter().map(|point| enu::values(*point)))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<List<EnuVector3, 64>, D::Error> {
        let points = Vec::<[f32; 3]>::deserialize(deserializer)?
            .into_iter()
            .map(enu::point)
            .collect::<Vec<_>>();
        List::from_slice(&points).map_err(serde::de::Error::custom)
    }
}

mod track {
    use super::*;
    pub fn serialize<S: Serializer>(
        path: &[EnuVector3; 2],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.map(enu::values).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[EnuVector3; 2], D::Error> {
        Ok(<[[f32; 3]; 2]>::deserialize(deserializer)?.map(enu::point))
    }
}

// Fixed storage keeps the existing Copy snapshot channel; JSON contains ordinary arrays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct List<T: Copy, const N: usize>([Option<T>; N]);

impl<T: Copy, const N: usize> Default for List<T, N> {
    fn default() -> Self {
        Self([None; N])
    }
}

impl<T: Copy, const N: usize> List<T, N> {
    pub fn from_slice(values: &[T]) -> Result<Self, String> {
        if values.len() > N {
            return Err(format!(
                "acoustic feed capacity {N} exceeded ({})",
                values.len()
            ));
        }
        let mut result = Self::default();
        for (slot, value) in result.0.iter_mut().zip(values) {
            *slot = Some(*value);
        }
        Ok(result)
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter().flatten()
    }
}

impl<T: Copy + Serialize, const N: usize> Serialize for List<T, N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de, T: Copy + Deserialize<'de>, const N: usize> Deserialize<'de> for List<T, N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_slice(&Vec::<T>::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Text([u8; 128], usize);

impl Text {
    pub fn new(text: &str) -> Self {
        let mut end = text.len().min(128);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut bytes = [0; 128];
        bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        Self(bytes, end)
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0[..self.1]).expect("feed text is UTF-8")
    }
}

impl Serialize for Text {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() > 128 {
            return Err(serde::de::Error::custom("feed text exceeds 128 bytes"));
        }
        Ok(Self::new(&text))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrivalKind {
    Crack,
    Direct,
    RoutedPrimary,
    Echo,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Arrival {
    pub kind: ArrivalKind,
    pub label: Text,
    #[serde(with = "polyline")]
    pub path_enu_m: List<EnuVector3, 64>,
    pub length_m: f64,
    /// True for an ordinary source's reconstructed street route, whose baked geometry is unavailable.
    pub path_is_prediction: bool,
    pub emission_time_s: f64,
    pub arrival_time_s: f64,
    /// Unit wave-travel direction toward the listener, in ENU.
    #[serde(with = "enu")]
    pub arrival_direction_enu: EnuVector3,
    pub band_pressure_gains: Option<[f32; 3]>,
    pub path_id: Option<u32>,
    pub facade_id: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crack {
    #[serde(with = "track")]
    pub flight_track_enu_m: [EnuVector3; 2],
    #[serde(with = "enu")]
    pub tangent_position_enu_m: EnuVector3,
    pub emission_time_s: f64,
    pub arrival_time_s: f64,
    pub mach: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcousticEvent {
    pub schema_version: u32,
    pub source_id: Text,
    pub source_index: usize,
    pub event_sequence: u64,
    pub sample_rate_hz: u32,
    pub trigger_audio_sample: u64,
    pub trigger_audio_time_s: f64,
    #[serde(with = "enu")]
    pub source_position_enu_m: EnuVector3,
    #[serde(with = "enu")]
    pub listener_position_enu_m: EnuVector3,
    pub source_emission_time_s: f64,
    pub line_of_sight: bool,
    pub arrivals: List<Arrival, 6>,
    pub crack: Option<Crack>,
}

impl AcousticEvent {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_plan(
        source_id: &str,
        source_index: usize,
        event_sequence: u64,
        sample_rate_hz: u32,
        trigger_audio_sample: u64,
        source: EnuVector3,
        listener: EnuVector3,
        emission_time_s: f64,
        plan: &crate::echo_paths::EchoPathPlan,
        host_rendered: bool,
        crack: Option<Crack>,
    ) -> Result<Self, String> {
        let mut arrivals = Vec::new();
        let mut add = |kind,
                       label: String,
                       path: &[EnuVector3],
                       length_m,
                       delay_m,
                       emission_time_s,
                       band_pressure_gains,
                       path_id,
                       facade_id| {
            let from = path.iter().rev().nth(1).copied().unwrap_or(source);
            arrivals.push(Arrival {
                kind,
                label: Text::new(&label),
                path_enu_m: List::from_slice(path)?,
                length_m,
                path_is_prediction: kind == ArrivalKind::RoutedPrimary && !host_rendered,
                emission_time_s,
                arrival_time_s: emission_time_s + delay_m / SPEED_OF_SOUND_MPS,
                arrival_direction_enu: direction(from, listener),
                band_pressure_gains,
                path_id,
                facade_id,
            });
            Ok::<(), String>(())
        };
        if plan.line_of_sight {
            add(
                ArrivalKind::Direct,
                "direct".into(),
                &[source, listener],
                plan.straight_line_m,
                plan.straight_line_m,
                emission_time_s,
                None,
                None,
                None,
            )?;
        } else if let Some(length_m) = plan.primary_route_m {
            let delay_m = if host_rendered {
                plan.timed_route
                    .map_or(plan.straight_line_m, |route| f64::from(route.length_m))
            } else {
                plan.straight_line_m
            };
            add(
                ArrivalKind::RoutedPrimary,
                if host_rendered {
                    format!(
                        "{} via street ({length_m:.0} m)",
                        if crack.is_some() { "impact" } else { "sound" }
                    )
                } else {
                    format!("sound via street (~{length_m:.0} m)")
                },
                &plan.primary_polyline_enu_m,
                length_m,
                delay_m,
                emission_time_s,
                None,
                None,
                None,
            )?;
        }
        if host_rendered {
            for tap in &plan.taps {
                // The facade id stays a field; labels are for listening.
                let label = match tap.facade_id {
                    Some(_) => "echo · building face".to_owned(),
                    None => "echo · around corner".into(),
                };
                add(
                    ArrivalKind::Echo,
                    label,
                    &tap.polyline_enu_m,
                    f64::from(tap.geometry.physical_path_length_m),
                    f64::from(tap.geometry.render_delay_path_m),
                    emission_time_s,
                    Some(tap.geometry.band_pressure_gain),
                    Some(tap.geometry.stable_path_id),
                    tap.facade_id,
                )?;
            }
        }
        if let Some(crack) = crack {
            let length = distance_m(crack.tangent_position_enu_m, listener);
            add(
                ArrivalKind::Crack,
                "crack".into(),
                &[crack.tangent_position_enu_m, listener],
                length,
                (crack.arrival_time_s - crack.emission_time_s) * SPEED_OF_SOUND_MPS,
                crack.emission_time_s,
                None,
                None,
                None,
            )?;
        }
        Ok(Self {
            schema_version: 1,
            source_id: Text::new(source_id),
            source_index,
            event_sequence,
            sample_rate_hz,
            trigger_audio_sample,
            trigger_audio_time_s: trigger_audio_sample as f64 / f64::from(sample_rate_hz),
            source_position_enu_m: source,
            listener_position_enu_m: listener,
            source_emission_time_s: emission_time_s,
            line_of_sight: plan.line_of_sight,
            arrivals: List::from_slice(&arrivals)?,
            crack,
        })
    }

    pub fn elapsed_s(&self, audio_sample: u64) -> f64 {
        audio_sample.saturating_sub(self.trigger_audio_sample) as f64
            / f64::from(self.sample_rate_hz)
    }

    pub fn timeline(&self) -> Vec<&Arrival> {
        let mut arrivals = self.arrivals.iter().collect::<Vec<_>>();
        arrivals.sort_by(|a, b| a.arrival_time_s.total_cmp(&b.arrival_time_s));
        arrivals
    }
}

pub fn ripple_radius_m(event: &AcousticEvent, audio_sample: u64) -> f64 {
    (event.elapsed_s(audio_sample) - event.source_emission_time_s).max(0.0) * SPEED_OF_SOUND_MPS
}

pub fn distance_m(a: EnuVector3, b: EnuVector3) -> f64 {
    let [x, y, z] = [a.east_m - b.east_m, a.north_m - b.north_m, a.up_m - b.up_m];
    (f64::from(x).powi(2) + f64::from(y).powi(2) + f64::from(z).powi(2)).sqrt()
}

pub fn direction(from: EnuVector3, to: EnuVector3) -> EnuVector3 {
    let length = distance_m(from, to).max(1.0e-12) as f32;
    EnuVector3::new(
        (to.east_m - from.east_m) / length,
        (to.north_m - from.north_m) / length,
        (to.up_m - from.up_m) / length,
    )
}

pub fn pulse_position_enu_m(arrival: &Arrival, elapsed_s: f64) -> Option<EnuVector3> {
    if elapsed_s < arrival.emission_time_s || elapsed_s > arrival.arrival_time_s {
        return None;
    }
    let points = arrival.path_enu_m.iter().copied().collect::<Vec<_>>();
    let total = points
        .windows(2)
        .map(|p| distance_m(p[0], p[1]))
        .sum::<f64>();
    // Usually duration = L/c. A render-horizon fallback follows its actual render delay.
    let duration = arrival.arrival_time_s - arrival.emission_time_s;
    let mut remaining = if duration > 0.0 {
        total * ((elapsed_s - arrival.emission_time_s) / duration).clamp(0.0, 1.0)
    } else {
        total
    };
    for segment in points.windows(2) {
        let length = distance_m(segment[0], segment[1]);
        if length > 0.0 && remaining <= length {
            let fraction = (remaining / length) as f32;
            let [a, b] = [segment[0], segment[1]];
            return Some(EnuVector3::new(
                a.east_m + (b.east_m - a.east_m) * fraction,
                a.north_m + (b.north_m - a.north_m) * fraction,
                a.up_m + (b.up_m - a.up_m) * fraction,
            ));
        }
        remaining -= length;
    }
    points.last().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> AcousticEvent {
        let source = EnuVector3::default();
        let listener = EnuVector3::new(343.0, 0.0, 0.0);
        AcousticEvent {
            schema_version: 1,
            source_id: Text::new("shot"),
            source_index: 0,
            event_sequence: 1,
            sample_rate_hz: 48_000,
            trigger_audio_sample: 48_000,
            trigger_audio_time_s: 1.0,
            source_position_enu_m: source,
            listener_position_enu_m: listener,
            source_emission_time_s: 0.25,
            line_of_sight: true,
            arrivals: List::from_slice(&[Arrival {
                kind: ArrivalKind::Direct,
                label: Text::new("direct"),
                path_enu_m: List::from_slice(&[source, listener]).unwrap(),
                length_m: 343.0,
                path_is_prediction: false,
                emission_time_s: 0.25,
                arrival_time_s: 1.25,
                arrival_direction_enu: direction(source, listener),
                band_pressure_gains: None,
                path_id: None,
                facade_id: None,
            }])
            .unwrap(),
            crack: None,
        }
    }

    #[test]
    fn ripple_radius_uses_audio_samples_and_emission_delay() {
        let event = event();
        assert_eq!(ripple_radius_m(&event, 0), 0.0);
        assert_eq!(ripple_radius_m(&event, 60_000), 0.0);
        assert!((ripple_radius_m(&event, 84_000) - 171.5).abs() < 1.0e-9);
    }

    #[test]
    fn pulse_follows_polyline_at_sound_speed() {
        let mut arrival = *event().arrivals.iter().next().unwrap();
        arrival.path_enu_m = List::from_slice(&[
            EnuVector3::default(),
            EnuVector3::default(),
            EnuVector3::new(100.0, 0.0, 0.0),
            EnuVector3::new(100.0, 243.0, 0.0),
        ])
        .unwrap();
        let point = pulse_position_enu_m(&arrival, 0.75).unwrap();
        assert!((point.east_m - 100.0).abs() < 1.0e-5);
        assert!((point.north_m - 71.5).abs() < 1.0e-5);
        assert!(pulse_position_enu_m(&arrival, 0.1).is_none());
        assert_eq!(
            pulse_position_enu_m(&arrival, 1.25),
            Some(EnuVector3::new(100.0, 243.0, 0.0))
        );
    }

    #[test]
    fn timeline_orders_plain_labels_for_synthetic_plan() {
        use crate::echo_paths::{EchoPathPlan, EchoRoute, PlannedPath};
        use fightbox_steam_audio::{EchoPathGeometry, EchoPathKind, PrimaryRoute};
        let source = EnuVector3::default();
        let listener = EnuVector3::new(300.0, 0.0, 0.0);
        let corner = EnuVector3::new(150.0, 100.0, 0.0);
        let plan = EchoPathPlan {
            straight_line_m: 300.0,
            primary_route_m: Some(559.0),
            timed_route: Some(PrimaryRoute {
                length_m: 559.0,
                topology_id: 1,
            }),
            line_of_sight: false,
            primary_arrival: corner,
            primary_polyline_enu_m: vec![source, corner, listener],
            primary_corners: 1,
            primary_turns: 1,
            candidates: 1,
            taps: vec![PlannedPath {
                route: EchoRoute::RoutedReflection {
                    entry_is_source: true,
                },
                geometry: EchoPathGeometry {
                    kind: EchoPathKind::Specular,
                    stable_path_id: 7,
                    physical_path_length_m: 686.0,
                    render_delay_path_m: 686.0,
                    arrival_position_enu: corner,
                    band_pressure_gain: [0.5; 3],
                },
                polyline_enu_m: vec![source, corner, listener],
                facade_id: Some(7),
                excess_s: 127.0 / SPEED_OF_SOUND_MPS,
                charged_corners: 0,
                predicted_pressure: 0.5,
            }],
        };
        let crack = Crack {
            flight_track_enu_m: [EnuVector3::new(1000.0, 0.0, 1000.0), source],
            tangent_position_enu_m: corner,
            emission_time_s: 0.25,
            arrival_time_s: 0.7,
            mach: 1.5,
        };
        let event = AcousticEvent::from_plan(
            "shot",
            0,
            1,
            48_000,
            48_000,
            source,
            listener,
            0.25,
            &plan,
            true,
            Some(crack),
        )
        .unwrap();
        assert_eq!(
            event
                .timeline()
                .iter()
                .map(|a| a.label.as_str())
                .collect::<Vec<_>>(),
            ["crack", "impact via street (559 m)", "echo · building face"]
        );
        assert!(
            !event
                .timeline()
                .iter()
                .any(|a| a.kind == ArrivalKind::Direct)
        );
        assert!((event.timeline()[1].arrival_time_s - (0.25 + 559.0 / 343.0)).abs() < 1.0e-9);
        let fallback = AcousticEvent::from_plan(
            "music", 0, 1, 48_000, 0, source, listener, 0.0, &plan, false, None,
        )
        .unwrap();
        let primary = fallback.arrivals.iter().next().unwrap();
        assert!(primary.path_is_prediction);
        assert_eq!(primary.label.as_str(), "sound via street (~559 m)");
        assert!((primary.arrival_time_s - 300.0 / 343.0).abs() < 1.0e-9);
    }

    #[test]
    fn feed_serialization_round_trip() {
        let event = event();
        let json = serde_json::to_string(&event).unwrap();
        let decoded: AcousticEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["arrivals"][0]["label"], "direct");
        assert_eq!(
            value["arrivals"][0]["path_enu_m"].as_array().unwrap().len(),
            2
        );
    }
}
