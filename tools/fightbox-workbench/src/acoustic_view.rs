use eframe::egui::{self, Align2, Color32, FontId, Painter, Pos2, Rect, Stroke};
use fightbox_api::EnuVector3;

use crate::acoustic_feed::{AcousticEvent, ArrivalKind, pulse_position_enu_m, ripple_radius_m};
use crate::ground_map::MapProjection;

const FLASH_S: f64 = 0.24;

fn arrival_color(kind: ArrivalKind) -> Color32 {
    match kind {
        ArrivalKind::Crack => Color32::from_rgb(255, 194, 97),
        ArrivalKind::Direct => Color32::from_rgb(109, 221, 244),
        ArrivalKind::RoutedPrimary => Color32::from_rgb(92, 244, 192),
        ArrivalKind::Echo => Color32::from_rgb(167, 174, 244),
    }
}

fn fade(color: Color32, amount: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (255.0 * amount.clamp(0.0, 1.0)) as u8,
    )
}

fn projected(projection: MapProjection, point: EnuVector3) -> Pos2 {
    projection.project([point.east_m, point.north_m])
}

fn flash_amount(elapsed_s: f64, arrival_time_s: f64) -> f32 {
    let age = elapsed_s - arrival_time_s;
    if (0.0..FLASH_S).contains(&age) {
        (1.0 - age / FLASH_S) as f32
    } else {
        0.0
    }
}

pub(crate) fn paint_map(
    painter: &Painter,
    map_rect: Rect,
    timeline_rect: Rect,
    projection: MapProjection,
    event: &AcousticEvent,
    audio_sample: u64,
) {
    if map_rect.width() < 1.0 || map_rect.height() < 1.0 {
        return;
    }
    let map = painter.with_clip_rect(map_rect);
    let elapsed_s = event.elapsed_s(audio_sample);
    let source = projected(projection, event.source_position_enu_m);
    let listener = projected(projection, event.listener_position_enu_m);
    let radius_m = ripple_radius_m(event, audio_sample);
    let scale = (projected(
        projection,
        EnuVector3::new(
            event.source_position_enu_m.east_m + 1.0,
            event.source_position_enu_m.north_m,
            event.source_position_enu_m.up_m,
        ),
    ) - source)
        .length();
    let radius_px = radius_m as f32 * scale;
    let ripple_alpha = (-radius_m / 420.0).exp() as f32;
    if radius_px > 0.5 && ripple_alpha > 0.015 {
        let color = fade(arrival_color(ArrivalKind::Direct), ripple_alpha);
        if event.line_of_sight {
            map.circle_stroke(source, radius_px, Stroke::new(1.8, color));
        } else {
            let stroke = Stroke::new(
                1.0,
                fade(arrival_color(ArrivalKind::Direct), ripple_alpha * 0.26),
            );
            for segment in 0..96 {
                if segment % 3 == 2 {
                    continue;
                }
                let angle = segment as f32 * std::f32::consts::TAU / 96.0;
                let next = (segment + 1) as f32 * std::f32::consts::TAU / 96.0;
                map.line_segment(
                    [
                        source + egui::vec2(angle.cos(), angle.sin()) * radius_px,
                        source + egui::vec2(next.cos(), next.sin()) * radius_px,
                    ],
                    stroke,
                );
            }
        }
    }

    if let Some(crack) = &event.crack {
        let color = arrival_color(ArrivalKind::Crack);
        let [start, end] = crack
            .flight_track_enu_m
            .map(|point| projected(projection, point));
        map.arrow(start, end - start, Stroke::new(1.0, fade(color, 0.40)));
        let tangent = projected(projection, crack.tangent_position_enu_m);
        map.line_segment([tangent, listener], Stroke::new(1.0, fade(color, 0.30)));
        map.circle_stroke(tangent, 3.5, Stroke::new(1.2, fade(color, 0.70)));
    }

    for arrival in event.arrivals.iter() {
        let color = arrival_color(arrival.kind);
        let echo = arrival.kind == ArrivalKind::Echo;
        let path_alpha = if echo { 0.28 } else { 0.58 };
        let points = arrival
            .path_enu_m
            .iter()
            .map(|point| projected(projection, *point))
            .collect::<Vec<_>>();
        for segment in points.windows(2) {
            map.line_segment(
                [segment[0], segment[1]],
                Stroke::new(if echo { 1.0 } else { 1.8 }, fade(color, path_alpha)),
            );
        }
        if audio_sample >= event.trigger_audio_sample {
            if let Some(point) = pulse_position_enu_m(arrival, elapsed_s) {
                let point = projected(projection, point);
                map.circle_filled(point, if echo { 6.0 } else { 9.0 }, fade(color, 0.13));
                map.circle_filled(
                    point,
                    if echo { 2.8 } else { 4.0 },
                    fade(color, if echo { 0.72 } else { 1.0 }),
                );
            }
            let flash = flash_amount(elapsed_s, arrival.arrival_time_s);
            if flash > 0.0 {
                map.circle_stroke(
                    listener,
                    5.0 + (1.0 - flash) * 21.0,
                    Stroke::new(if echo { 1.5 } else { 2.5 }, fade(color, flash)),
                );
                map.circle_filled(listener, 4.0 + flash * 3.0, fade(color, flash));
            }
        }
    }
    paint_timeline(painter, timeline_rect, event, audio_sample);
}

fn paint_timeline(painter: &Painter, rect: Rect, event: &AcousticEvent, audio_sample: u64) {
    if rect.width() < 32.0 || rect.height() < 32.0 {
        return;
    }
    let painter = painter.with_clip_rect(rect);
    painter.rect_filled(rect, 4.0, Color32::from_rgba_unmultiplied(8, 14, 21, 238));
    let elapsed_s = event.elapsed_s(audio_sample);
    let arrivals = event.timeline();
    let span_s = arrivals
        .last()
        .map_or(2.5, |arrival| (arrival.arrival_time_s + 0.1).max(2.5));
    let left = rect.left() + 8.0;
    let right = rect.right() - 8.0;
    let axis_y = rect.top() + 27.0;
    let x_at = |time_s: f64| left + ((time_s / span_s).clamp(0.0, 1.0) as f32) * (right - left);
    painter.text(
        Pos2::new(left, rect.top() + 5.0),
        Align2::LEFT_TOP,
        format!("Arrivals · {:.2} s", elapsed_s),
        FontId::proportional(11.0),
        Color32::from_rgb(203, 220, 232),
    );
    painter.text(
        Pos2::new(right, rect.top() + 6.0),
        Align2::RIGHT_TOP,
        format!("0–{span_s:.1} s"),
        FontId::monospace(9.0),
        Color32::from_rgb(123, 149, 166),
    );
    painter.line_segment(
        [Pos2::new(left, axis_y), Pos2::new(right, axis_y)],
        Stroke::new(1.0, Color32::from_rgb(75, 95, 109)),
    );
    let playhead = x_at(elapsed_s);
    painter.line_segment(
        [
            Pos2::new(playhead, axis_y - 7.0),
            Pos2::new(playhead, rect.bottom() - 4.0),
        ],
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(220, 236, 243, 100)),
    );
    // Echoes closer than 60 px share one label, so a cluster cannot stack rows.
    let mut labels: Vec<Option<String>> = arrivals
        .iter()
        .map(|arrival| Some(arrival.label.as_str().to_owned()))
        .collect();
    let mut first = 0;
    while first < arrivals.len() {
        let mut end = first + 1;
        if arrivals[first].kind == ArrivalKind::Echo {
            let x0 = x_at(arrivals[first].arrival_time_s);
            while end < arrivals.len()
                && arrivals[end].kind == ArrivalKind::Echo
                && x_at(arrivals[end].arrival_time_s) - x0 < 60.0
            {
                labels[end] = None;
                end += 1;
            }
            if end - first > 1 {
                labels[first] = Some(format!("{} echoes", end - first));
            }
        }
        first = end;
    }
    let mut row_ends = Vec::<f32>::new();
    for (arrival, label) in arrivals.iter().zip(labels) {
        let passed =
            audio_sample >= event.trigger_audio_sample && elapsed_s >= arrival.arrival_time_s;
        let color = fade(arrival_color(arrival.kind), if passed { 1.0 } else { 0.46 });
        let tick_x = x_at(arrival.arrival_time_s);
        let flash = flash_amount(elapsed_s, arrival.arrival_time_s);
        painter.line_segment(
            [
                Pos2::new(tick_x, axis_y - 5.0),
                Pos2::new(tick_x, axis_y + 5.0),
            ],
            Stroke::new(if passed { 2.0 } else { 1.0 }, color),
        );
        painter.circle_filled(Pos2::new(tick_x, axis_y), 2.0 + flash * 3.0, color);
        let Some(mut label) = label else {
            continue;
        };
        let font = FontId::proportional(10.0);
        let mut galley = painter.layout_no_wrap(label.clone(), font.clone(), color);
        while galley.size().x > right - left && label.chars().count() > 4 {
            label.pop();
            galley = painter.layout_no_wrap(format!("{label}…"), font.clone(), color);
        }
        let label_x =
            (tick_x - galley.size().x * 0.5).clamp(left, (right - galley.size().x).max(left));
        let row = row_ends
            .iter()
            .position(|end| label_x >= *end + 6.0)
            .unwrap_or(row_ends.len());
        if row == row_ends.len() {
            row_ends.push(label_x + galley.size().x);
        } else {
            row_ends[row] = label_x + galley.size().x;
        }
        let label_y = axis_y + 10.0 + row as f32 * 13.0;
        painter.line_segment(
            [
                Pos2::new(tick_x, axis_y + 5.0),
                Pos2::new(tick_x, label_y - 2.0),
            ],
            Stroke::new(0.7, fade(arrival_color(arrival.kind), 0.18)),
        );
        painter.galley(Pos2::new(label_x, label_y), galley, color);
    }
}

pub(crate) fn paint_first_person(
    painter: &Painter,
    rect: Rect,
    event: &AcousticEvent,
    audio_sample: u64,
    yaw_radians: f32,
) {
    let timeline_height = (44.0 + event.arrivals.iter().count() as f32 * 13.0)
        .min(122.0)
        .min(rect.height() * 0.45);
    let timeline_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 8.0, rect.bottom() - timeline_height - 8.0),
        rect.right_bottom() - egui::vec2(8.0, 8.0),
    );
    let edge_rect = Rect::from_min_max(
        rect.min + egui::vec2(15.0, 15.0),
        Pos2::new(rect.right() - 15.0, timeline_rect.top() - 10.0),
    );
    if edge_rect.width() > 1.0
        && edge_rect.height() > 1.0
        && audio_sample >= event.trigger_audio_sample
    {
        let elapsed_s = event.elapsed_s(audio_sample);
        let painter = painter.with_clip_rect(rect);
        for arrival in event.arrivals.iter() {
            let flash = flash_amount(elapsed_s, arrival.arrival_time_s);
            if flash == 0.0 {
                continue;
            }
            // Pose yaw is clockwise from north; a wave points opposite its heard bearing.
            let wave = arrival.arrival_direction_enu;
            let bearing = (-wave.east_m).atan2(-wave.north_m) - yaw_radians;
            let direction = egui::vec2(bearing.sin(), -bearing.cos());
            let distance = (edge_rect.width() * 0.5 / direction.x.abs().max(1.0e-6))
                .min(edge_rect.height() * 0.5 / direction.y.abs().max(1.0e-6));
            let point = edge_rect.center() + direction * distance;
            let color = fade(arrival_color(arrival.kind), flash);
            painter.circle_filled(
                point,
                7.0 + flash * 7.0,
                fade(arrival_color(arrival.kind), flash * 0.20),
            );
            painter.line_segment(
                [
                    point - direction.rot90() * 10.0,
                    point + direction.rot90() * 10.0,
                ],
                Stroke::new(3.5, color),
            );
            painter.circle_filled(point, 3.0, color);
        }
    }
    paint_timeline(painter, timeline_rect, event, audio_sample);
}
