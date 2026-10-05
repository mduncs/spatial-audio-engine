//! LineSegment-first Wave 11 width presentation.
//!
//! A logical line source keeps one physical propagation trajectory. After that
//! shared transport, the admitted polyphase allpass pair produces a center arm
//! and a quadrature arm. The three presentation feeds are center, +Q, and -Q;
//! only their Steam Audio binaural directions differ.

use crate::SteamVector3;
use crate::propagation_delay::TELEPORT_CROSSFADE_SECONDS;

/// Frozen identifier of the candidate accepted by the Wave 11 listening gate.
pub(crate) const WIDTH_RENDERER_REVISION: &str = "polyphase-iir-ap3x3-c0p015-v1";
/// ABX-selected maximum phase angle. This is deliberately not configurable.
pub(crate) const PHI_MAX_RADIANS: f32 = core::f32::consts::FRAC_PI_4;
/// The admitted allpass pair has current-sample feedthrough in both arms.
pub(crate) const DECLARED_LATENCY_SAMPLES: u32 = 0;

// Ported verbatim from width_binaural_prototype.rs. Even coefficients form Q;
// odd coefficients form C, whose conventional z^-1 is replaced by the
// a=0.015 first-order allpass so the declared integer latency remains zero.
pub(crate) const QUADRATURE_COEFFICIENTS: [f64; 3] = [
    0.135_955_273_394_143_46,
    0.675_584_032_663_335_2,
    0.927_402_314_224_353_3,
];
pub(crate) const CENTER_COEFFICIENTS: [f64; 3] = [
    0.421_648_251_942_761_3,
    0.837_382_617_956_687_6,
    0.979_448_541_496_229_2,
];
pub(crate) const CENTER_FIRST_ORDER_COEFFICIENT: f64 = 0.015;

#[derive(Clone, Copy, Debug, Default)]
struct Allpass2 {
    coefficient: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Allpass2 {
    fn new(coefficient: f64) -> Self {
        Self {
            coefficient,
            ..Self::default()
        }
    }

    #[inline]
    fn process(&mut self, input: f64) -> f64 {
        // H(z) = (c - z^-2) / (1 - c z^-2).
        let output = self.coefficient * (input + self.y2) - self.x2;
        self.x2 = self.x1;
        self.x1 = input;
        self.y2 = self.y1;
        self.y1 = output;
        output
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Allpass1 {
    coefficient: f64,
    x1: f64,
    y1: f64,
}

impl Allpass1 {
    fn new(coefficient: f64) -> Self {
        Self {
            coefficient,
            ..Self::default()
        }
    }

    #[inline]
    fn process(&mut self, input: f64) -> f64 {
        // H(z) = (a + z^-1) / (1 + a z^-1).
        let output = self.coefficient * input + self.x1 - self.coefficient * self.y1;
        self.x1 = input;
        self.y1 = output;
        output
    }
}

#[derive(Debug)]
struct PhaseSplitter {
    center: [Allpass2; 3],
    center_first_order: Allpass1,
    quadrature: [Allpass2; 3],
}

impl PhaseSplitter {
    fn new() -> Self {
        Self {
            center: CENTER_COEFFICIENTS.map(Allpass2::new),
            center_first_order: Allpass1::new(CENTER_FIRST_ORDER_COEFFICIENT),
            quadrature: QUADRATURE_COEFFICIENTS.map(Allpass2::new),
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> (f32, f32) {
        let mut center = f64::from(input);
        for section in &mut self.center {
            center = section.process(center);
        }
        center = self.center_first_order.process(center);

        let mut quadrature = f64::from(input);
        for section in &mut self.quadrature {
            quadrature = section.process(quadrature);
        }
        (center as f32, quadrature as f32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LineGeometry {
    pub(crate) center: SteamVector3,
    pub(crate) plus_endpoint: SteamVector3,
    pub(crate) minus_endpoint: SteamVector3,
    pub(crate) k: f32,
    pub(crate) phi_eff_radians: f32,
}

/// Computes endpoint directions and the exact angular-subtense collapse law.
///
/// `source_forward` is a validated unit vector in Steam coordinates. Endpoint
/// positions are used only for binaural directions; callers must never use them
/// to derive propagation delay, Doppler, physical gain, or simulation inputs.
pub(crate) fn line_geometry(
    center: SteamVector3,
    source_forward: SteamVector3,
    listener: SteamVector3,
    length_m: f32,
) -> LineGeometry {
    let half_extent_m = 0.5 * length_m;
    let plus_endpoint = add_scaled(center, source_forward, half_extent_m);
    let minus_endpoint = add_scaled(center, source_forward, -half_extent_m);
    let plus = subtract(plus_endpoint, listener);
    let minus = subtract(minus_endpoint, listener);
    let plus_length_squared = dot(plus, plus);
    let minus_length_squared = dot(minus, minus);

    // Listener/endpoint coincidence is excluded by the authored-fixture
    // contract. Keep production finite if an unsanitized live pose reaches it:
    // the intersecting segment occupies the maximum angular width.
    let k = if plus_length_squared <= 1.0e-12 || minus_length_squared <= 1.0e-12 {
        1.0
    } else {
        let plus_unit = scale(plus, plus_length_squared.sqrt().recip());
        let minus_unit = scale(minus, minus_length_squared.sqrt().recip());
        let cross_length = dot(cross(minus_unit, plus_unit), cross(minus_unit, plus_unit))
            .max(0.0)
            .sqrt();
        let omega = cross_length.atan2(dot(minus_unit, plus_unit).clamp(-1.0, 1.0));
        (0.5 * omega).sin().clamp(0.0, 1.0)
    };

    LineGeometry {
        center,
        plus_endpoint,
        minus_endpoint,
        k,
        phi_eff_radians: PHI_MAX_RADIANS * k,
    }
}

#[derive(Debug)]
struct WidthSlew {
    initialized: bool,
    current_k: f32,
    target_k: f32,
    teleport_remaining: u32,
    teleport_frames: u32,
}

impl WidthSlew {
    fn new(sample_rate_hz: i32) -> Self {
        let teleport_frames = (TELEPORT_CROSSFADE_SECONDS * sample_rate_hz as f32)
            .ceil()
            .max(1.0) as u32;
        Self {
            initialized: false,
            current_k: 0.0,
            target_k: 0.0,
            teleport_remaining: 0,
            teleport_frames,
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn reset(&mut self) {
        self.initialized = false;
        self.current_k = 0.0;
        self.target_k = 0.0;
        self.teleport_remaining = 0;
    }

    fn begin_block(&mut self, target_k: f32, teleported: bool) {
        let target_k = target_k.clamp(0.0, 1.0);
        if !self.initialized {
            self.initialized = true;
            self.current_k = target_k;
            self.target_k = target_k;
            self.teleport_remaining = 0;
            return;
        }
        self.target_k = target_k;
        if teleported {
            self.teleport_remaining = self.teleport_frames;
        }
    }

    #[inline]
    fn next_teleport_sample(&mut self) -> f32 {
        if self.teleport_remaining == 0 {
            return self.current_k;
        }
        self.current_k += (self.target_k - self.current_k) / self.teleport_remaining.max(1) as f32;
        self.teleport_remaining -= 1;
        if self.teleport_remaining == 0 {
            self.current_k = self.target_k;
        }
        self.current_k
    }
}

/// Per-logical-source state for the admitted mono LineSegment renderer.
#[derive(Debug)]
pub(crate) struct LineWidthRenderer {
    splitter: PhaseSplitter,
    slew: WidthSlew,
}

impl LineWidthRenderer {
    pub(crate) fn new(sample_rate_hz: i32) -> Self {
        Self {
            splitter: PhaseSplitter::new(),
            slew: WidthSlew::new(sample_rate_hz),
        }
    }

    #[cfg(feature = "linked-sdk")]
    pub(crate) fn reset(&mut self) {
        self.splitter = PhaseSplitter::new();
        self.slew.reset();
    }

    /// Writes interleaved `[C, +Q, -Q]` presentation feeds.
    ///
    /// Ordinary simulation motion ramps to the new geometric endpoint over the
    /// current block. A propagation-head teleport instead uses exactly the
    /// established 50 ms teleport crossfade duration and persists across blocks.
    pub(crate) fn render_presentation(
        &mut self,
        input: &[f32],
        target_k: f32,
        teleported: bool,
        output: &mut [f32],
    ) {
        assert_eq!(output.len(), input.len() * 3);
        self.slew.begin_block(target_k, teleported);
        let normal_start = self.slew.current_k;
        let normal_target = self.slew.target_k;
        let normal_ramp = self.slew.teleport_remaining == 0;
        let frames = input.len();

        for (frame, (&input, feeds)) in input.iter().zip(output.chunks_exact_mut(3)).enumerate() {
            let k = if self.slew.teleport_remaining > 0 {
                self.slew.next_teleport_sample()
            } else if normal_ramp && frames > 0 {
                let progress = (frame + 1) as f32 / frames as f32;
                normal_start + (normal_target - normal_start) * progress
            } else {
                self.slew.current_k
            };
            let phi = PHI_MAX_RADIANS * k.clamp(0.0, 1.0);
            let (cosine, sine) = (phi.cos(), phi.sin());
            let (center, quadrature) = self.splitter.process(input);
            feeds[0] = cosine * center;
            feeds[1] = sine * quadrature;
            feeds[2] = -sine * quadrature;
        }
        if normal_ramp {
            self.slew.current_k = normal_target;
        }
    }

    #[cfg(test)]
    fn current_k(&self) -> f32 {
        self.slew.current_k
    }

    #[cfg(test)]
    fn is_teleport_slewing(&self) -> bool {
        self.slew.teleport_remaining > 0
    }
}

fn add_scaled(origin: SteamVector3, direction: SteamVector3, scale: f32) -> SteamVector3 {
    SteamVector3::new(
        origin.x + direction.x * scale,
        origin.y + direction.y * scale,
        origin.z + direction.z * scale,
    )
}

fn subtract(left: SteamVector3, right: SteamVector3) -> SteamVector3 {
    SteamVector3::new(left.x - right.x, left.y - right.y, left.z - right.z)
}

fn scale(vector: SteamVector3, scale: f32) -> SteamVector3 {
    SteamVector3::new(vector.x * scale, vector.y * scale, vector.z * scale)
}

fn dot(left: SteamVector3, right: SteamVector3) -> f32 {
    left.x * right.x + left.y * right.y + left.z * right.z
}

fn cross(left: SteamVector3, right: SteamVector3) -> SteamVector3 {
    SteamVector3::new(
        left.y * right.z - left.z * right.y,
        left.z * right.x - left.x * right.z,
        left.x * right.y - left.y * right.x,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_scalar_pair(center: f32, quadrature: f32, phi: f32) -> (f32, f32) {
        (
            phi.cos() * center + phi.sin() * quadrature,
            phi.cos() * center - phi.sin() * quadrature,
        )
    }

    #[test]
    fn broadside_subtense_collapses_monotonically_and_presentation_is_equal_power() {
        let center = SteamVector3::default();
        let forward = SteamVector3::new(1.0, 0.0, 0.0);
        let half_extent = 3.0_f32;
        let mut previous = f32::INFINITY;
        for step in 0..=375 {
            let distance = 2.5 + step as f32 * 0.1;
            let geometry = line_geometry(
                center,
                forward,
                SteamVector3::new(0.0, 0.0, distance),
                2.0 * half_extent,
            );
            let analytic = half_extent / (distance * distance + half_extent * half_extent).sqrt();
            assert!(geometry.k.is_finite());
            assert!((geometry.k - analytic).abs() <= 2.0e-6);
            assert!(geometry.k < previous);
            previous = geometry.k;
        }

        let matched = 0.375_f32;
        let expected_power = 2.0 * matched * matched;
        for step in 0..=180 {
            let phi = PHI_MAX_RADIANS * step as f32 / 180.0;
            let (left, right) = render_scalar_pair(matched, matched, phi);
            let power = left * left + right * right;
            assert!((power - expected_power).abs() <= 2.0e-7);
        }
    }

    #[test]
    fn admitted_center_arm_has_zero_integer_latency() {
        let mut splitter = PhaseSplitter::new();
        let mut center = [0.0_f32; 16];
        for (index, sample) in center.iter_mut().enumerate() {
            *sample = splitter.process(if index == 0 { 1.0 } else { 0.0 }).0;
        }

        assert_eq!(DECLARED_LATENCY_SAMPLES, 0);
        assert_ne!(center[0].to_bits(), 0.0_f32.to_bits());
        let bypass = [1.0_f32, 0.0];
        assert_eq!(
            center.iter().position(|sample| sample.abs() > 1.0e-12),
            bypass.iter().position(|sample| sample.abs() > 1.0e-12),
            "the C arm must begin on the same sample as an undelayed bypass"
        );
        let peak = center
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))
            .map(|(index, _)| index);
        assert_eq!(
            peak,
            Some(3),
            "the signed candidate's energy peak is part of its impulse contract, not D_q"
        );
    }

    #[test]
    fn teleport_width_slew_engages_and_finishes_within_the_shared_window() {
        let sample_rate_hz = 1_000;
        let fade_frames = (TELEPORT_CROSSFADE_SECONDS * sample_rate_hz as f32).ceil() as usize;
        let mut renderer = LineWidthRenderer::new(sample_rate_hz);
        let mut output = [0.0_f32; 3];
        renderer.render_presentation(&[1.0], 0.1, false, &mut output);
        assert_eq!(renderer.current_k().to_bits(), 0.1_f32.to_bits());

        renderer.render_presentation(&[1.0], 0.9, true, &mut output);
        assert!(renderer.is_teleport_slewing());
        assert!(renderer.current_k() > 0.1 && renderer.current_k() < 0.9);
        for _ in 1..fade_frames {
            renderer.render_presentation(&[0.0], 0.9, false, &mut output);
        }
        assert!(!renderer.is_teleport_slewing());
        assert_eq!(renderer.current_k().to_bits(), 0.9_f32.to_bits());
    }

    #[test]
    fn production_coefficients_match_the_admitted_prototype_exactly() {
        let expected_quadrature = [
            0.135_955_273_394_143_46_f64,
            0.675_584_032_663_335_2,
            0.927_402_314_224_353_3,
        ];
        let expected_center = [
            0.421_648_251_942_761_3_f64,
            0.837_382_617_956_687_6,
            0.979_448_541_496_229_2,
        ];
        assert_eq!(
            QUADRATURE_COEFFICIENTS.map(f64::to_bits),
            expected_quadrature.map(f64::to_bits)
        );
        assert_eq!(
            CENTER_COEFFICIENTS.map(f64::to_bits),
            expected_center.map(f64::to_bits)
        );
        assert_eq!(
            CENTER_FIRST_ORDER_COEFFICIENT.to_bits(),
            0.015_f64.to_bits()
        );
        assert_eq!(WIDTH_RENDERER_REVISION, "polyphase-iir-ap3x3-c0p015-v1");
    }
}
