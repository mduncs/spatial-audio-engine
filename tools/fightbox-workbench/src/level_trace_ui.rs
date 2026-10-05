//! UI-thread wrapper for the real output-level trace channel.
//!
//! `level_trace::LevelTraceWriter` remains the callback-side producer. This
//! module owns only the reader's drain/history presentation and export controls.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2};
use serde_json::Value;

use crate::level_trace::{LevelSample, LevelTraceReader};

/// Result of the compact control row; root starts/stops the callback writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceUiAction {
    None,
    Record,
    Stop,
}

/// Bounded presentation state around the callback-safe `LevelTraceReader`.
pub struct LevelTraceUi {
    pub reader: LevelTraceReader,
    recording: bool,
}

impl LevelTraceUi {
    pub fn new(reader: LevelTraceReader) -> Self {
        Self {
            reader,
            recording: false,
        }
    }
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// Drain is UI/control-thread only. Returns drained count for status text.
    pub fn update(&mut self) -> usize {
        self.reader.drain()
    }

    /// Record/Stop toggle. No audio start, device selection, or autoplay occurs here.
    pub fn controls(&mut self, ui: &mut egui::Ui, can_record: bool) -> TraceUiAction {
        let mut action = TraceUiAction::None;
        ui.horizontal_wrapped(|ui| {
            ui.label("Output level · measured");
            if self.recording {
                if ui.button("■ Stop trace").clicked() {
                    self.recording = false;
                    action = TraceUiAction::Stop;
                }
            } else if ui
                .add_enabled(can_record, egui::Button::new("● Record trace"))
                .clicked()
            {
                self.recording = true;
                action = TraceUiAction::Record;
            }
            if ui
                .add_enabled(
                    !self.recording && !self.reader.is_recording(),
                    egui::Button::new("Clear"),
                )
                .clicked()
            {
                if let Err(error) = self.reader.clear_history(false) {
                    ui.small(error);
                }
            }
            ui.small(format!(
                "100 ms · {} windows · {} queue drops · {} evicted",
                self.reader.samples().len(),
                self.reader.dropped_windows(),
                self.reader.evicted_windows()
            ));
        });
        action
    }

    /// Exports the reader's canonical JSON plus caller-owned identity metadata
    /// to a unique create-new file under the external capture root.
    pub fn export_json(&self, root: &Path, metadata: Value) -> io::Result<PathBuf> {
        fs::create_dir_all(root)?;
        let core: Value =
            serde_json::from_str(&self.reader.export_json().map_err(io::Error::other)?)
                .map_err(io::Error::other)?;
        let payload = serde_json::json!({
            "schema_version": "fightbox.output-level-trace-with-metadata.v1",
            "measured_signal": "summed final stereo output PCM after limiter",
            "core": core,
            "metadata": metadata,
        });
        let bytes = serde_json::to_vec_pretty(&payload).map_err(io::Error::other)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        for serial in 0..1000u32 {
            let path = root.join(format!("level-trace-{stamp}-{serial:03}.json"));
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            return Ok(path);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate unique trace filename",
        ))
    }

    /// Draws measured listener trail and the 100-ms time strip. The projection
    /// is supplied by ground_map; no source-phase or acoustic-field inference occurs.
    pub fn paint<F>(&self, painter: &egui::Painter, map_rect: Rect, mut project: F, time_rect: Rect)
    where
        F: FnMut([f32; 2]) -> Option<Pos2>,
    {
        let clipped = painter.with_clip_rect(map_rect);
        let mut previous: Option<(&LevelSample, Pos2)> = None;
        let mut nearest: Option<(&LevelSample, Pos2, f32)> = None;
        for sample in self.reader.samples() {
            // Invalid pose/PCM is retained for diagnosis but never projected.
            let Some(pos) = sample
                .pose_valid
                .then(|| project([sample.listener_end_m[0], sample.listener_end_m[1]]))
                .flatten()
            else {
                previous = None;
                continue;
            };
            let continuous = previous
                .as_ref()
                .map(|(old, _)| sample.can_connect_from(old))
                .unwrap_or(false);
            let color = level_color(sample);
            if let Some((_, old_pos)) = previous {
                if continuous {
                    clipped.line_segment([old_pos, pos], Stroke::new(2.0, color));
                }
            }
            clipped.circle_filled(pos, 3.0, color);
            if is_drop_flag(sample, previous.map(|(p, _)| p)) {
                clipped.circle_stroke(pos, 6.0, Stroke::new(1.5, Color32::from_rgb(238, 151, 45)));
            }
            if let Some(pointer) = painter.ctx().input(|i| i.pointer.hover_pos()) {
                let distance = pointer.distance(pos);
                if nearest.map(|(_, _, d)| distance < d).unwrap_or(true) {
                    nearest = Some((sample, pos, distance));
                }
            }
            previous = Some((sample, pos));
        }
        painter.rect_stroke(
            map_rect,
            2.0,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(100, 220, 180, 130)),
            egui::StrokeKind::Inside,
        );
        painter.text(
            map_rect.left_top() + Vec2::new(6.0, 6.0),
            egui::Align2::LEFT_TOP,
            "Output level · measured | orange = ≥6 dB possible drop (natural decay also counts)",
            egui::FontId::proportional(11.0),
            Color32::LIGHT_GRAY,
        );
        if let Some((sample, pos, _distance)) = nearest.filter(|(_, _, d)| *d < 18.0) {
            let label = sample_label(sample);
            let text = format!(
                "{label}\nframes {}–{} · ENU {:.1}, {:.1}, {:.1}\nsource phase unavailable",
                sample.start_frame,
                sample.end_frame,
                sample.listener_end_m[0],
                sample.listener_end_m[1],
                sample.listener_end_m[2]
            );
            let box_rect = Rect::from_min_size(pos + Vec2::new(8.0, -56.0), Vec2::new(245.0, 52.0));
            painter.rect_filled(
                box_rect,
                3.0,
                Color32::from_rgba_unmultiplied(8, 14, 19, 235),
            );
            painter.text(
                box_rect.left_top() + Vec2::splat(5.0),
                egui::Align2::LEFT_TOP,
                text,
                egui::FontId::proportional(11.0),
                Color32::WHITE,
            );
        }
        self.paint_time_strip(painter, time_rect);
    }

    fn paint_time_strip(&self, painter: &egui::Painter, rect: Rect) {
        painter.rect_filled(rect, 2.0, Color32::from_rgb(17, 23, 29));
        let Some(first) = self.reader.samples().front() else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "No measured trace",
                egui::FontId::default(),
                Color32::GRAY,
            );
            return;
        };
        let min_t = first.start_frame as f32 / self.reader.sample_rate_hz() as f32;
        let max_t = self
            .reader
            .samples()
            .back()
            .map(|s| s.end_frame as f32 / self.reader.sample_rate_hz() as f32)
            .unwrap_or(min_t)
            .max(min_t + 0.1);
        for sample in self.reader.samples() {
            let x = rect.left()
                + (((sample.start_frame as f32 / self.reader.sample_rate_hz() as f32) - min_t)
                    / (max_t - min_t))
                    .clamp(0.0, 1.0)
                    * rect.width();
            let rms = sample.rms_dbfs().unwrap_or(-120.0) as f32;
            let peak = sample.peak_dbfs().unwrap_or(-120.0) as f32;
            let rms_height = ((rms + 60.0) / 60.0).clamp(0.04, 1.0) * rect.height();
            let peak_height = ((peak + 60.0) / 60.0).clamp(0.04, 1.0) * rect.height();
            painter.line_segment(
                [
                    Pos2::new(x, rect.bottom()),
                    Pos2::new(x, rect.bottom() - rms_height),
                ],
                Stroke::new(2.0, level_color(sample)),
            );
            painter.line_segment(
                [
                    Pos2::new(x, rect.bottom()),
                    Pos2::new(x, rect.bottom() - peak_height),
                ],
                Stroke::new(1.0, Color32::WHITE),
            );
        }
        painter.text(
            rect.left_top() + Vec2::new(4.0, 3.0),
            egui::Align2::LEFT_TOP,
            format!("{min_t:.1}s–{max_t:.1}s RMS / peak"),
            egui::FontId::proportional(11.0),
            Color32::LIGHT_GRAY,
        );
        if let Some(pointer) = painter
            .ctx()
            .input(|i| i.pointer.hover_pos())
            .filter(|p| rect.contains(*p))
        {
            let frame = (min_t
                + (pointer.x - rect.left()) / rect.width().max(1.0) * (max_t - min_t))
                as f64
                * f64::from(self.reader.sample_rate_hz());
            if let Some(sample) = self
                .reader
                .samples()
                .iter()
                .find(|s| frame >= s.start_frame as f64 && frame < s.end_frame as f64)
            {
                let text = format!(
                    "{} · {:.2}s · ENU {:.1}, {:.1}, {:.1}",
                    sample_label(sample),
                    sample.start_frame as f64 / f64::from(self.reader.sample_rate_hz()),
                    sample.listener_end_m[0],
                    sample.listener_end_m[1],
                    sample.listener_end_m[2]
                );
                painter.text(
                    rect.left_bottom() - Vec2::new(-4.0, 3.0),
                    egui::Align2::LEFT_BOTTOM,
                    text,
                    egui::FontId::proportional(11.0),
                    Color32::WHITE,
                );
            }
        }
    }
}

fn level_color(sample: &LevelSample) -> Color32 {
    if sample.nonfinite_frames != 0 || !sample.pose_valid {
        return Color32::from_rgb(170, 75, 180);
    }
    if sample.exact_silence {
        return Color32::from_rgb(85, 125, 150);
    }
    let t = sample
        .rms_dbfs()
        .map(|db| ((db + 60.0) / 60.0).clamp(0.0, 1.0) as f32)
        .unwrap_or(0.0);
    Color32::from_rgb((70.0 + 185.0 * t) as u8, (190.0 - 95.0 * t) as u8, 120)
}

fn sample_label(sample: &LevelSample) -> String {
    if sample.nonfinite_frames != 0 {
        return "invalid PCM / pose".into();
    }
    if !sample.pose_valid {
        return "invalid pose".into();
    }
    if sample.exact_silence {
        return "silence · RMS −∞ · peak −∞".into();
    }
    format!(
        "RMS {:.1} dBFS · peak {:.1} dBFS",
        sample.rms_dbfs().unwrap_or(-120.0),
        sample.peak_dbfs().unwrap_or(-120.0)
    )
}

fn is_drop_flag(sample: &LevelSample, previous: Option<&LevelSample>) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if !sample.can_connect_from(previous)
        || !matches!(sample.end_reason, crate::level_trace::WindowEnd::Complete)
        || !matches!(previous.end_reason, crate::level_trace::WindowEnd::Complete)
        || sample.nonfinite_frames != 0
        || previous.nonfinite_frames != 0
        || !sample.pose_valid
        || !previous.pose_valid
    {
        return false;
    }
    let before = previous.rms_linear.unwrap_or(0.0);
    let after = sample.rms_linear.unwrap_or(0.0);
    before > 1.0e-3 && (20.0 * (before / after.max(f64::MIN_POSITIVE)).log10()) >= 6.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level_trace::{TraceIdentity, WindowEnd};

    fn sample(seq: u64, start: u64, rms: Option<f64>, silence: bool) -> LevelSample {
        LevelSample {
            sequence: seq,
            start_frame: start,
            end_frame: start + 4_800,
            identity: TraceIdentity {
                generation: 1,
                epoch: 1,
            },
            listener_start_m: [0.0; 3],
            listener_end_m: [0.0; 3],
            pose_valid: true,
            segment_start: seq == 0,
            end_reason: WindowEnd::Complete,
            exact_silence: silence,
            nonfinite_frames: 0,
            rms_linear: rms,
            peak_linear: rms,
            gap_before: 0,
        }
    }

    #[test]
    fn exact_silence_after_finite_is_a_possible_drop() {
        let before = sample(1, 4_800, Some(1.0), false);
        let after = sample(2, 9_600, Some(0.0), true);
        assert!(is_drop_flag(&after, Some(&before)));
    }

    #[test]
    fn identity_gap_and_partial_windows_are_not_flagged() {
        let before = sample(1, 4_800, Some(1.0), false);
        let mut identity = sample(2, 9_600, Some(0.001), false);
        identity.identity.epoch = 2;
        assert!(!is_drop_flag(&identity, Some(&before)));
        let mut gap = sample(3, 14_400, Some(0.001), false);
        gap.gap_before = 1;
        assert!(!is_drop_flag(&gap, Some(&before)));
        let mut partial = sample(2, 9_600, Some(0.001), false);
        partial.end_reason = WindowEnd::Flushed;
        assert!(!is_drop_flag(&partial, Some(&before)));
    }

    #[test]
    fn invalid_pcm_and_pose_are_not_flagged() {
        let before = sample(1, 4_800, Some(1.0), false);
        let mut invalid_pcm = sample(2, 9_600, Some(0.001), false);
        invalid_pcm.nonfinite_frames = 1;
        assert!(!is_drop_flag(&invalid_pcm, Some(&before)));
        let mut invalid_pose = sample(2, 9_600, Some(0.001), false);
        invalid_pose.pose_valid = false;
        assert!(!is_drop_flag(&invalid_pose, Some(&before)));
    }
}
