use super::ffi;
use super::neutral_environment::{
    MAX_NEUTRAL_ENVIRONMENT_CHANNELS, active_channel_count, steam_environment_frame_to_neutral,
};

const SAMPLE_RATE: i32 = 48_000;
const FRAME_SIZE: i32 = 128;

#[test]
fn pinned_unspatialized_path_effect_emits_world_space_acn_prefixes() {
    let context = TestContext::create();
    let coefficients = [
        1.0, -0.5, 0.25, -0.125, 0.0625, -0.03125, 0.015625, -0.0078125, 0.00390625,
    ];
    let identity_listener = ffi::IPLCoordinateSpace3 {
        right: vector(1.0, 0.0, 0.0),
        up: vector(0.0, 1.0, 0.0),
        ahead: vector(0.0, 0.0, -1.0),
        origin: vector(0.0, 0.0, 0.0),
    };
    let yawed_and_translated_listener = ffi::IPLCoordinateSpace3 {
        right: vector(0.0, 0.0, -1.0),
        up: vector(0.0, 1.0, 0.0),
        ahead: vector(-1.0, 0.0, 0.0),
        origin: vector(17.0, -3.0, 29.0),
    };

    for order in 0..=2 {
        let identity = render_unspatialized_path(&context, order, coefficients, identity_listener);
        let yawed =
            render_unspatialized_path(&context, order, coefficients, yawed_and_translated_listener);

        assert_eq!(
            samples_as_bits(&identity),
            samples_as_bits(&yawed),
            "unspatialized order-{order} output must remain in world space"
        );

        let active = active_channel_count(order).unwrap();
        assert!(
            identity
                .chunks_exact(MAX_NEUTRAL_ENVIRONMENT_CHANNELS)
                .any(|frame| frame[0].abs() > 1.0e-6),
            "Steam returned a silent order-{order} characterization block"
        );

        for (frame_index, frame) in identity
            .chunks_exact(MAX_NEUTRAL_ENVIRONMENT_CHANNELS)
            .enumerate()
        {
            let reference = frame[0];
            for channel in 0..active {
                let expected = reference * coefficients[channel];
                assert!(
                    (frame[channel] - expected).abs() <= 1.0e-6,
                    "order {order}, frame {frame_index}, ACN {channel}: {} != {expected}",
                    frame[channel]
                );
            }
            for (channel, sample) in frame[active..].iter().copied().enumerate() {
                assert_eq!(
                    sample.to_bits(),
                    0,
                    "order {order}, frame {frame_index}, inactive ACN {} was not cleared",
                    active + channel
                );
            }

            let raw: [f32; MAX_NEUTRAL_ENVIRONMENT_CHANNELS] = frame.try_into().unwrap();
            let mut neutral = [f32::NAN; MAX_NEUTRAL_ENVIRONMENT_CHANNELS];
            steam_environment_frame_to_neutral(order, &raw, &mut neutral).unwrap();
            assert_eq!(samples_as_bits(&neutral), samples_as_bits(frame));
        }
    }
}

fn render_unspatialized_path(
    context: &TestContext,
    order: i32,
    mut coefficients: [f32; MAX_NEUTRAL_ENVIRONMENT_CHANNELS],
    listener: ffi::IPLCoordinateSpace3,
) -> Vec<f32> {
    let mut audio_settings = ffi::IPLAudioSettings {
        samplingRate: SAMPLE_RATE,
        frameSize: FRAME_SIZE,
    };
    let effect = TestPathEffect::create(context, &mut audio_settings);
    let mut input = TestAudioBuffer::allocate(context, 1, FRAME_SIZE);
    let mut output =
        TestAudioBuffer::allocate(context, MAX_NEUTRAL_ENVIRONMENT_CHANNELS as i32, FRAME_SIZE);

    let mut input_samples = (0..FRAME_SIZE)
        .map(|sample| ((sample % 23) as f32 - 11.0) / 16.0)
        .collect::<Vec<_>>();
    input.write_interleaved(&mut input_samples);

    let mut params = ffi::IPLPathEffectParams {
        eqCoeffs: [1.0; 3],
        shCoeffs: coefficients.as_mut_ptr(),
        order,
        binaural: ffi::IPL_FALSE,
        hrtf: core::ptr::null_mut(),
        listener,
        normalizeEQ: ffi::IPL_FALSE,
    };
    ffi::path_effect_apply(effect.raw, &mut params, &mut input.raw, &mut output.raw);

    output.read_interleaved()
}

fn vector(x: f32, y: f32, z: f32) -> ffi::IPLVector3 {
    ffi::IPLVector3 { x, y, z }
}

fn samples_as_bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|sample| sample.to_bits()).collect()
}

struct TestContext {
    raw: ffi::IPLContext,
}

impl TestContext {
    fn create() -> Self {
        let mut settings = ffi::IPLContextSettings::pinned_defaults();
        let mut raw = core::ptr::null_mut();
        let status = ffi::context_create(&mut settings, &mut raw);
        assert_eq!(status, ffi::IPL_STATUS_SUCCESS, "iplContextCreate failed");
        assert!(!raw.is_null(), "iplContextCreate returned a null handle");
        Self { raw }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        ffi::context_release(&mut self.raw);
    }
}

struct TestPathEffect<'context> {
    raw: ffi::IPLPathEffect,
    _context: &'context TestContext,
}

impl<'context> TestPathEffect<'context> {
    fn create(context: &'context TestContext, audio_settings: &mut ffi::IPLAudioSettings) -> Self {
        let mut settings = ffi::IPLPathEffectSettings {
            maxOrder: 2,
            spatialize: ffi::IPL_FALSE,
            speakerLayout: ffi::IPLSpeakerLayout {
                type_: ffi::IPL_SPEAKERLAYOUTTYPE_STEREO,
                numSpeakers: 0,
                speakers: core::ptr::null_mut(),
            },
            hrtf: core::ptr::null_mut(),
        };
        let mut raw = core::ptr::null_mut();
        let status = ffi::path_effect_create(context.raw, audio_settings, &mut settings, &mut raw);
        assert_eq!(
            status,
            ffi::IPL_STATUS_SUCCESS,
            "iplPathEffectCreate failed"
        );
        assert!(!raw.is_null(), "iplPathEffectCreate returned a null handle");
        Self {
            raw,
            _context: context,
        }
    }
}

impl Drop for TestPathEffect<'_> {
    fn drop(&mut self) {
        ffi::path_effect_release(&mut self.raw);
    }
}

struct TestAudioBuffer<'context> {
    raw: ffi::IPLAudioBuffer,
    context: &'context TestContext,
}

impl<'context> TestAudioBuffer<'context> {
    fn allocate(context: &'context TestContext, channels: i32, samples: i32) -> Self {
        let mut raw = ffi::IPLAudioBuffer {
            numChannels: 0,
            numSamples: 0,
            data: core::ptr::null_mut(),
        };
        let status = ffi::audio_buffer_allocate(context.raw, channels, samples, &mut raw);
        assert_eq!(
            status,
            ffi::IPL_STATUS_SUCCESS,
            "iplAudioBufferAllocate failed"
        );
        assert_eq!(raw.numChannels, channels);
        assert_eq!(raw.numSamples, samples);
        assert!(!raw.data.is_null());
        Self { raw, context }
    }

    fn write_interleaved(&mut self, samples: &mut [f32]) {
        ffi::audio_buffer_deinterleave(self.context.raw, samples, &mut self.raw);
    }

    fn read_interleaved(&mut self) -> Vec<f32> {
        let mut samples = vec![0.0; self.raw.numChannels as usize * self.raw.numSamples as usize];
        ffi::audio_buffer_interleave(self.context.raw, &mut self.raw, &mut samples);
        samples
    }
}

impl Drop for TestAudioBuffer<'_> {
    fn drop(&mut self) {
        ffi::audio_buffer_free(self.context.raw, &mut self.raw);
    }
}
