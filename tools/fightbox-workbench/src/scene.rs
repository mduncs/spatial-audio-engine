//! Scene cues compiled off-thread and performed by the input audio clock.

use fightbox_api::EnuVector3;
use fightbox_runtime::MAX_ACTIVE_SOURCES;

use crate::fixture::{Fixture, FixtureCueZone};

pub(crate) const SCENE_BLOCK_FRAMES: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SceneControl {
    pub generation: u64,
    pub running: bool,
    pub listener: EnuVector3,
    pub prepared_generations: [u64; MAX_ACTIVE_SOURCES],
    pub delay_frames: [u32; MAX_ACTIVE_SOURCES],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SceneStatus {
    pub generation: u64,
    pub running: bool,
    pub frame: u64,
    pub start_audio_sample: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SceneFrame {
    pub enabled: bool,
    pub play: bool,
    pub generation: u64,
}

struct TimeCue {
    frame: u64,
    source: usize,
    play: bool,
}

struct ZoneCue {
    zone: FixtureCueZone,
    source: usize,
    fired: bool,
}

pub(crate) struct SceneTimeline {
    times: Vec<TimeCue>,
    zones: Vec<ZoneCue>,
    next: usize,
    enabled: [bool; MAX_ACTIVE_SOURCES],
    generations: [u64; MAX_ACTIVE_SOURCES],
    pub status: SceneStatus,
    pub block: [[SceneFrame; SCENE_BLOCK_FRAMES]; MAX_ACTIVE_SOURCES],
}

impl SceneTimeline {
    pub fn new(fixture: &Fixture, sample_rate: u32) -> Self {
        let mut times = Vec::new();
        let mut zones = Vec::new();
        for cue in &fixture.cues {
            let source = fixture
                .sources
                .iter()
                .position(|source| source.id == cue.source_id())
                .expect("fixture validated cue source");
            if let Some(time) = cue.at_s {
                times.push(TimeCue {
                    frame: (time * f64::from(sample_rate)).round() as u64,
                    source,
                    play: cue.play.is_some(),
                });
            } else if let Some(zone) = cue.when_listener_enters {
                zones.push(ZoneCue {
                    zone,
                    source,
                    fired: false,
                });
            }
        }
        // Stable order preserves file order for simultaneous time cues.
        times.sort_by_key(|cue| cue.frame);
        Self {
            times,
            zones,
            next: 0,
            enabled: [false; MAX_ACTIVE_SOURCES],
            generations: [0; MAX_ACTIVE_SOURCES],
            status: SceneStatus::default(),
            block: [[SceneFrame::default(); SCENE_BLOCK_FRAMES]; MAX_ACTIVE_SOURCES],
        }
    }

    /// Returns true when playback must rewind. No allocation, locks or host time.
    pub fn begin_block(&mut self, control: SceneControl, audio_sample: u64) -> bool {
        let reset =
            control.generation != self.status.generation || control.running != self.status.running;
        if reset {
            self.next = 0;
            self.enabled.fill(false);
            for zone in &mut self.zones {
                zone.fired = false;
            }
            self.status = SceneStatus {
                generation: control.generation,
                running: control.running,
                start_audio_sample: audio_sample,
                frame: 0,
            };
        }
        for offset in 0..SCENE_BLOCK_FRAMES {
            let mut played = [false; MAX_ACTIVE_SOURCES];
            if control.running {
                if offset == 0 {
                    for cue in &mut self.zones {
                        let east = f64::from(control.listener.east_m) - cue.zone.center_m[0];
                        let north = f64::from(control.listener.north_m) - cue.zone.center_m[1];
                        if !cue.fired && east.hypot(north) <= cue.zone.radius_m {
                            cue.fired = true;
                            self.enabled[cue.source] = true;
                            self.generations[cue.source] =
                                self.generations[cue.source].wrapping_add(1);
                            played[cue.source] = true;
                        }
                    }
                }
                while let Some(cue) = self.times.get(self.next) {
                    if cue.frame > self.status.frame + offset as u64 {
                        break;
                    }
                    self.enabled[cue.source] = cue.play;
                    if cue.play {
                        self.generations[cue.source] = self.generations[cue.source].wrapping_add(1);
                        played[cue.source] = true;
                    }
                    self.next += 1;
                }
            }
            for source in 0..MAX_ACTIVE_SOURCES {
                self.block[source][offset] = SceneFrame {
                    enabled: control.running && self.enabled[source],
                    play: played[source],
                    generation: self.generations[source],
                };
            }
        }
        if control.running {
            self.status.frame += SCENE_BLOCK_FRAMES as u64;
        }
        reset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(cues: serde_json::Value) -> SceneTimeline {
        let mut wire: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/city/astra-artillery/street-path-candidate.json"
        ))
        .unwrap();
        wire["cues"] = cues;
        let fixture = Fixture::parse(&serde_json::to_vec(&wire).unwrap(), "scene-test").unwrap();
        SceneTimeline::new(&fixture, 48_000)
    }

    #[test]
    fn scene_time_offsets_cross_blocks_and_stop_resets() {
        let mut timeline = scene(serde_json::json!([
            {"at_s": 127.0 / 48_000.0, "play": "artillery-corner-shot"},
            {"at_s": 129.0 / 48_000.0, "stop": "artillery-corner-shot"},
            {"at_s": 257.0 / 48_000.0, "play": "artillery-corner-shot"}
        ]));
        let mut control = SceneControl::default();
        timeline.begin_block(control, 0);
        assert!(
            timeline.block[0]
                .iter()
                .all(|frame| !frame.enabled && !frame.play)
        );
        control.running = true;
        control.generation = 1;
        assert!(timeline.begin_block(control, 128));
        assert!(timeline.block[0][..127].iter().all(|frame| !frame.enabled));
        assert!(timeline.block[0][127].enabled && timeline.block[0][127].play);
        timeline.begin_block(control, 256);
        assert!(timeline.block[0][0].enabled);
        assert!(!timeline.block[0][1].enabled);
        timeline.begin_block(control, 384);
        assert!(!timeline.block[0][0].enabled);
        assert!(timeline.block[0][1].enabled && timeline.block[0][1].play);
        assert!(
            timeline.block[1].iter().all(|frame| !frame.enabled),
            "uncued source stays off"
        );
        control.running = false;
        assert!(timeline.begin_block(control, 512));
        assert_eq!(timeline.status.frame, 0);
        assert!(timeline.block.iter().flatten().all(|frame| !frame.enabled));
        control.running = true;
        control.generation += 1;
        timeline.begin_block(control, 640);
        assert_eq!(timeline.status.start_audio_sample, 640);
        assert!(timeline.block[0][127].play, "new Play rewinds the timeline");
    }

    #[test]
    fn scene_zone_latches_across_boundary_jitter_and_rearms_after_stop() {
        let mut timeline = scene(serde_json::json!([
            {"when_listener_enters": {"center_m": [0,0], "radius_m": 2}, "play": "artillery-corner-shot"}
        ]));
        let mut control = SceneControl {
            generation: 1,
            running: true,
            listener: EnuVector3::new(2.01, 0.0, 1.5),
            ..SceneControl::default()
        };
        timeline.begin_block(control, 0);
        assert!(!timeline.block[0][0].play);
        for (block, east) in [2.0, 2.01, 1.99, 2.0, 0.0].into_iter().enumerate() {
            control.listener.east_m = east;
            timeline.begin_block(control, (block as u64 + 1) * 128);
            assert_eq!(timeline.block[0][0].play, block == 0);
        }
        control.running = false;
        timeline.begin_block(control, 768);
        control.running = true;
        control.generation += 1;
        timeline.begin_block(control, 896);
        assert!(timeline.block[0][0].play);
    }
}
