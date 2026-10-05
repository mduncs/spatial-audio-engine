#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudioTypes.h>
#include <TargetConditionals.h>

#include <math.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum {
  kSpikeBlockFrames = 128,
  kSpikeRenderFrames = 4096,
  kSpikeMaximumChannels = 9,
  kSpikeMaximumObjectBuses = 48,
  kSpikeMaximumInputBuses = kSpikeMaximumObjectBuses + 1,
};

typedef struct {
  UInt32 activeBus;
  UInt32 impulseChannel;
  Boolean emitted[kSpikeMaximumInputBuses];
  Boolean seen[kSpikeMaximumInputBuses];
  Float32 scratch[kSpikeMaximumChannels][kSpikeBlockFrames];
} SpikeRenderContext;

typedef struct {
  UInt32 mNumberBuffers;
  AudioBuffer mBuffers[2];
} SpikeStereoBufferList;

typedef struct {
  OSStatus formatStatus;
  OSStatus layoutStatus;
  OSStatus initializeStatus;
  OSStatus renderStatus;
  Float64 reportedLatencySeconds;
  UInt32 firstNonzeroFrame;
  UInt32 peakFrame;
  Float32 peakMagnitude;
  UInt32 inputBusCount;
  UInt32 callbacksSeen;
  OSStatus policyReadbackStatus;
  OSStatus headTrackingSetStatus;
  OSStatus personalizedModeSetStatus;
  OSStatus personalizedActiveGetStatus;
  UInt32 personalizedActive;
  UInt32 internalReverb;
  UInt32 objectRenderingFlags;
  UInt32 environmentRenderingFlags;
  Float32 objectReverbBlend;
  Float32 environmentReverbBlend;
  Float32 objectOcclusion;
  Float32 environmentOcclusion;
  Float32 objectObstruction;
  Float32 environmentObstruction;
} SpikeProbeResult;

static AudioStreamBasicDescription spike_float_format(UInt32 channels) {
  AudioStreamBasicDescription format = {0};
  format.mSampleRate = 48000.0;
  format.mFormatID = kAudioFormatLinearPCM;
  format.mFormatFlags =
      kAudioFormatFlagsNativeFloatPacked | kAudioFormatFlagIsNonInterleaved;
  format.mBytesPerPacket = sizeof(Float32);
  format.mFramesPerPacket = 1;
  format.mBytesPerFrame = sizeof(Float32);
  format.mChannelsPerFrame = channels;
  format.mBitsPerChannel = 8 * sizeof(Float32);
  return format;
}

static size_t spike_tag_layout_size(void) {
  return offsetof(AudioChannelLayout, mChannelDescriptions);
}

static OSStatus spike_set_tag_layout(AudioUnit unit, AudioUnitScope scope,
                                     AudioUnitElement element,
                                     AudioChannelLayoutTag tag) {
  AudioChannelLayout layout = {0};
  layout.mChannelLayoutTag = tag;
  return AudioUnitSetProperty(unit, kAudioUnitProperty_AudioChannelLayout,
                              scope, element, &layout,
                              (UInt32)spike_tag_layout_size());
}

static OSStatus spike_render_callback(void *refCon,
                                      AudioUnitRenderActionFlags *actionFlags,
                                      const AudioTimeStamp *timeStamp,
                                      UInt32 bus, UInt32 frameCount,
                                      AudioBufferList *ioData) {
  (void)actionFlags;
  (void)timeStamp;
  SpikeRenderContext *context = (SpikeRenderContext *)refCon;
  if (bus >= kSpikeMaximumInputBuses || frameCount > kSpikeBlockFrames) {
    return kAudio_ParamError;
  }
  context->seen[bus] = true;

  for (UInt32 bufferIndex = 0; bufferIndex < ioData->mNumberBuffers;
       ++bufferIndex) {
    AudioBuffer *buffer = &ioData->mBuffers[bufferIndex];
    if (buffer->mData == NULL) {
      if (bufferIndex >= kSpikeMaximumChannels) {
        return kAudio_ParamError;
      }
      buffer->mData = context->scratch[bufferIndex];
      buffer->mDataByteSize = frameCount * (UInt32)sizeof(Float32);
    }
    memset(buffer->mData, 0, buffer->mDataByteSize);
  }

  if (bus == context->activeBus && !context->emitted[bus]) {
    if (context->impulseChannel >= ioData->mNumberBuffers ||
        ioData->mBuffers[context->impulseChannel].mData == NULL) {
      return kAudio_ParamError;
    }
    Float32 *samples =
        (Float32 *)ioData->mBuffers[context->impulseChannel].mData;
    samples[0] = 1.0f;
    context->emitted[bus] = true;
  }
  return noErr;
}

static void spike_dispose(AudioUnit unit) {
  if (unit != NULL) {
    AudioUnitUninitialize(unit);
    AudioComponentInstanceDispose(unit);
  }
}

static OSStatus spike_set_u32_property(AudioUnit unit,
                                       AudioUnitPropertyID property,
                                       AudioUnitScope scope,
                                       AudioUnitElement element, UInt32 value) {
  return AudioUnitSetProperty(unit, property, scope, element, &value,
                              sizeof(value));
}

static OSStatus spike_get_u32_property(AudioUnit unit,
                                       AudioUnitPropertyID property,
                                       AudioUnitScope scope,
                                       AudioUnitElement element,
                                       UInt32 *value) {
  UInt32 size = sizeof(*value);
  return AudioUnitGetProperty(unit, property, scope, element, value, &size);
}

static OSStatus spike_get_parameter(AudioUnit unit,
                                    AudioUnitParameterID parameter,
                                    AudioUnitScope scope,
                                    AudioUnitElement element, Float32 *value) {
  return AudioUnitGetParameter(unit, parameter, scope, element, value);
}

static OSStatus spike_configure_input_policy(AudioUnit unit, UInt32 bus,
                                             UInt32 mode) {
  OSStatus status =
      spike_set_u32_property(unit, kAudioUnitProperty_SpatialMixerSourceMode,
                             kAudioUnitScope_Input, bus, mode);
  if (status != noErr) {
    return status;
  }

  UInt32 algorithm = kSpatializationAlgorithm_UseOutputType;
  status =
      spike_set_u32_property(unit, kAudioUnitProperty_SpatializationAlgorithm,
                             kAudioUnitScope_Input, bus, algorithm);
  if (status != noErr) {
    return status;
  }

  /* Steam owns distance. Keep only Apple's interaural delay operation. */
  UInt32 flags = kSpatialMixerRenderingFlags_InterAuralDelay;
  status = spike_set_u32_property(unit,
                                  kAudioUnitProperty_SpatialMixerRenderingFlags,
                                  kAudioUnitScope_Input, bus, flags);
  if (status != noErr) {
    return status;
  }

  const struct {
    AudioUnitParameterID parameter;
    AudioUnitParameterValue value;
  } parameters[] = {
      {kSpatialMixerParam_Distance, 1.0f},
      {kSpatialMixerParam_ReverbBlend, 0.0f},
      {kSpatialMixerParam_OcclusionAttenuation, 0.0f},
      {kSpatialMixerParam_ObstructionAttenuation, 0.0f},
  };
  for (size_t index = 0; index < sizeof(parameters) / sizeof(parameters[0]);
       ++index) {
    status = AudioUnitSetParameter(unit, parameters[index].parameter,
                                   kAudioUnitScope_Input, bus,
                                   parameters[index].value, 0);
    if (status != noErr) {
      return status;
    }
  }
  return noErr;
}

static AudioUnit spike_create_unit(OSStatus *statusOut) {
  AudioComponentDescription description = {
      .componentType = kAudioUnitType_Mixer,
      .componentSubType = kAudioUnitSubType_SpatialMixer,
      .componentManufacturer = kAudioUnitManufacturer_Apple,
      .componentFlags = 0,
      .componentFlagsMask = 0,
  };
  AudioComponent component = AudioComponentFindNext(NULL, &description);
  if (component == NULL) {
    *statusOut = kAudio_ParamError;
    return NULL;
  }
  AudioUnit unit = NULL;
  *statusOut = AudioComponentInstanceNew(component, &unit);
  return *statusOut == noErr ? unit : NULL;
}

static SpikeProbeResult spike_probe(AudioChannelLayoutTag environmentTag,
                                    UInt32 environmentChannels,
                                    UInt32 objectBusCount, UInt32 impulseBus,
                                    UInt32 impulseChannel) {
  SpikeProbeResult result = {
      .formatStatus = kAudio_ParamError,
      .layoutStatus = kAudio_ParamError,
      .initializeStatus = kAudio_ParamError,
      .renderStatus = kAudio_ParamError,
      .reportedLatencySeconds = -1.0,
      .firstNonzeroFrame = UINT32_MAX,
      .peakFrame = UINT32_MAX,
      .peakMagnitude = 0.0f,
      .inputBusCount = 0,
      .callbacksSeen = 0,
      .policyReadbackStatus = kAudio_UnimplementedError,
      .headTrackingSetStatus = kAudio_UnimplementedError,
      .personalizedModeSetStatus = kAudio_UnimplementedError,
      .personalizedActiveGetStatus = kAudio_UnimplementedError,
      .personalizedActive = 0,
  };

  if (objectBusCount == 0 || objectBusCount > kSpikeMaximumObjectBuses ||
      environmentChannels == 0 || environmentChannels > kSpikeMaximumChannels) {
    return result;
  }
  const UInt32 environmentBus = objectBusCount;
  result.inputBusCount = objectBusCount + 1;

  OSStatus status = noErr;
  AudioUnit unit = spike_create_unit(&status);
  if (unit == NULL) {
    result.formatStatus = status;
    return result;
  }

  UInt32 inputCount = objectBusCount + 1;
  status = AudioUnitSetProperty(unit, kAudioUnitProperty_ElementCount,
                                kAudioUnitScope_Input, 0, &inputCount,
                                sizeof(inputCount));
  if (status != noErr) {
    result.formatStatus = status;
    spike_dispose(unit);
    return result;
  }

  UInt32 maximumFrames = kSpikeBlockFrames;
  status =
      spike_set_u32_property(unit, kAudioUnitProperty_MaximumFramesPerSlice,
                             kAudioUnitScope_Global, 0, maximumFrames);
  if (status != noErr) {
    result.formatStatus = status;
    spike_dispose(unit);
    return result;
  }

  AudioStreamBasicDescription monoFormat = spike_float_format(1);
  AudioStreamBasicDescription environmentFormat =
      spike_float_format(environmentChannels);
  AudioStreamBasicDescription stereoFormat = spike_float_format(2);

  for (UInt32 bus = 0; status == noErr && bus < objectBusCount; ++bus) {
    status = AudioUnitSetProperty(unit, kAudioUnitProperty_StreamFormat,
                                  kAudioUnitScope_Input, bus, &monoFormat,
                                  sizeof(monoFormat));
  }
  if (status == noErr) {
    status = AudioUnitSetProperty(
        unit, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input,
        environmentBus, &environmentFormat, sizeof(environmentFormat));
  }
  if (status == noErr) {
    status = AudioUnitSetProperty(unit, kAudioUnitProperty_StreamFormat,
                                  kAudioUnitScope_Output, 0, &stereoFormat,
                                  sizeof(stereoFormat));
  }
  result.formatStatus = status;
  if (status != noErr) {
    spike_dispose(unit);
    return result;
  }

  for (UInt32 bus = 0; status == noErr && bus < objectBusCount; ++bus) {
    status = spike_set_tag_layout(unit, kAudioUnitScope_Input, bus,
                                  kAudioChannelLayoutTag_Mono);
  }
  if (status == noErr) {
    status = spike_set_tag_layout(unit, kAudioUnitScope_Input, environmentBus,
                                  environmentTag);
  }
  if (status == noErr) {
    status = spike_set_tag_layout(unit, kAudioUnitScope_Output, 0,
                                  kAudioChannelLayoutTag_Stereo);
  }
  result.layoutStatus = status;
  if (status != noErr) {
    spike_dispose(unit);
    return result;
  }

  UInt32 outputType = kSpatialMixerOutputType_Headphones;
  status =
      spike_set_u32_property(unit, kAudioUnitProperty_SpatialMixerOutputType,
                             kAudioUnitScope_Global, 0, outputType);
  if (status == noErr) {
    UInt32 internalReverb = 0;
    status = spike_set_u32_property(unit, kAudioUnitProperty_UsesInternalReverb,
                                    kAudioUnitScope_Global, 0, internalReverb);
  }
  for (UInt32 bus = 0; status == noErr && bus < objectBusCount; ++bus) {
    status = spike_configure_input_policy(unit, bus,
                                          kSpatialMixerSourceMode_PointSource);
  }
  if (status == noErr) {
    status = spike_configure_input_policy(unit, environmentBus,
                                          kSpatialMixerSourceMode_AmbienceBed);
  }

  if (__builtin_available(macOS 13.0, iOS 18.0, tvOS 18.0, *)) {
    UInt32 enableHeadTracking = 1;
    result.headTrackingSetStatus = spike_set_u32_property(
        unit, kAudioUnitProperty_SpatialMixerEnableHeadTracking,
        kAudioUnitScope_Global, 0, enableHeadTracking);
    UInt32 personalizedMode = kSpatialMixerPersonalizedHRTFMode_Auto;
    result.personalizedModeSetStatus = spike_set_u32_property(
        unit, kAudioUnitProperty_SpatialMixerPersonalizedHRTFMode,
        kAudioUnitScope_Global, 0, personalizedMode);
  }

  if (status != noErr) {
    result.initializeStatus = status;
    spike_dispose(unit);
    return result;
  }

  SpikeRenderContext context = {
      .activeBus = impulseBus,
      .impulseChannel = impulseChannel,
  };
  AURenderCallbackStruct callback = {
      .inputProc = spike_render_callback,
      .inputProcRefCon = &context,
  };
  for (UInt32 bus = 0; bus < inputCount; ++bus) {
    status = AudioUnitSetProperty(unit, kAudioUnitProperty_SetRenderCallback,
                                  kAudioUnitScope_Input, bus, &callback,
                                  sizeof(callback));
    if (status != noErr) {
      result.initializeStatus = status;
      spike_dispose(unit);
      return result;
    }
  }

  status = AudioUnitInitialize(unit);
  result.initializeStatus = status;
  if (status != noErr) {
    spike_dispose(unit);
    return result;
  }

  if (__builtin_available(macOS 14.0, iOS 18.0, tvOS 18.0, *)) {
    result.personalizedActiveGetStatus = spike_get_u32_property(
        unit, kAudioUnitProperty_SpatialMixerAnyInputIsUsingPersonalizedHRTF,
        kAudioUnitScope_Global, 0, &result.personalizedActive);
  }

  status =
      spike_get_u32_property(unit, kAudioUnitProperty_UsesInternalReverb,
                             kAudioUnitScope_Global, 0, &result.internalReverb);
  if (status == noErr) {
    status = spike_get_u32_property(
        unit, kAudioUnitProperty_SpatialMixerRenderingFlags,
        kAudioUnitScope_Input, 0, &result.objectRenderingFlags);
  }
  if (status == noErr) {
    status = spike_get_u32_property(
        unit, kAudioUnitProperty_SpatialMixerRenderingFlags,
        kAudioUnitScope_Input, environmentBus,
        &result.environmentRenderingFlags);
  }
  const struct {
    AudioUnitParameterID parameter;
    Float32 *objectValue;
    Float32 *environmentValue;
  } policyParameters[] = {
      {
          kSpatialMixerParam_ReverbBlend,
          &result.objectReverbBlend,
          &result.environmentReverbBlend,
      },
      {
          kSpatialMixerParam_OcclusionAttenuation,
          &result.objectOcclusion,
          &result.environmentOcclusion,
      },
      {
          kSpatialMixerParam_ObstructionAttenuation,
          &result.objectObstruction,
          &result.environmentObstruction,
      },
  };
  for (size_t index = 0;
       status == noErr &&
       index < sizeof(policyParameters) / sizeof(policyParameters[0]);
       ++index) {
    status = spike_get_parameter(unit, policyParameters[index].parameter,
                                 kAudioUnitScope_Input, 0,
                                 policyParameters[index].objectValue);
    if (status == noErr) {
      status = spike_get_parameter(unit, policyParameters[index].parameter,
                                   kAudioUnitScope_Input, environmentBus,
                                   policyParameters[index].environmentValue);
    }
  }
  result.policyReadbackStatus = status;

  UInt32 latencySize = sizeof(result.reportedLatencySeconds);
  status = AudioUnitGetProperty(unit, kAudioUnitProperty_Latency,
                                kAudioUnitScope_Global, 0,
                                &result.reportedLatencySeconds, &latencySize);
  if (status != noErr) {
    result.reportedLatencySeconds = -1.0;
  }

  Float32 *left = calloc(kSpikeRenderFrames, sizeof(Float32));
  Float32 *right = calloc(kSpikeRenderFrames, sizeof(Float32));
  if (left == NULL || right == NULL) {
    free(left);
    free(right);
    spike_dispose(unit);
    return result;
  }

  result.renderStatus = noErr;
  for (UInt32 offset = 0; offset < kSpikeRenderFrames;
       offset += kSpikeBlockFrames) {
    SpikeStereoBufferList output = {
        .mNumberBuffers = 2,
        .mBuffers =
            {
                {
                    .mNumberChannels = 1,
                    .mDataByteSize =
                        kSpikeBlockFrames * (UInt32)sizeof(Float32),
                    .mData = left + offset,
                },
                {
                    .mNumberChannels = 1,
                    .mDataByteSize =
                        kSpikeBlockFrames * (UInt32)sizeof(Float32),
                    .mData = right + offset,
                },
            },
    };
    AudioTimeStamp timestamp = {0};
    timestamp.mSampleTime = offset;
    timestamp.mFlags = kAudioTimeStampSampleTimeValid;
    AudioUnitRenderActionFlags flags = 0;
    status = AudioUnitRender(unit, &flags, &timestamp, 0, kSpikeBlockFrames,
                             (AudioBufferList *)&output);
    if (status != noErr) {
      result.renderStatus = status;
      break;
    }
  }

  if (result.renderStatus == noErr) {
    const Float32 threshold = 1.0e-7f;
    for (UInt32 frame = 0; frame < kSpikeRenderFrames; ++frame) {
      Float32 magnitude = fmaxf(fabsf(left[frame]), fabsf(right[frame]));
      if (result.firstNonzeroFrame == UINT32_MAX && magnitude > threshold) {
        result.firstNonzeroFrame = frame;
      }
      if (magnitude > result.peakMagnitude) {
        result.peakMagnitude = magnitude;
        result.peakFrame = frame;
      }
    }
  }
  for (UInt32 bus = 0; bus < result.inputBusCount; ++bus) {
    if (context.seen[bus]) {
      ++result.callbacksSeen;
    }
  }

  free(left);
  free(right);
  spike_dispose(unit);
  return result;
}

static void spike_print_status(OSStatus status) {
  UInt32 bits = (UInt32)status;
  char fourCC[5] = {
      (char)((bits >> 24) & 0xff),
      (char)((bits >> 16) & 0xff),
      (char)((bits >> 8) & 0xff),
      (char)(bits & 0xff),
      '\0',
  };
  for (size_t index = 0; index < 4; ++index) {
    if (fourCC[index] < 32 || fourCC[index] > 126) {
      fourCC[index] = '.';
    }
  }
  printf("%d ('%s')", (int)status, fourCC);
}

static void spike_print_result(const char *label, AudioChannelLayoutTag tag,
                               UInt32 channels,
                               const SpikeProbeResult *result) {
  printf("%s tag=0x%08x channels=%u\n", label, (unsigned int)tag,
         (unsigned int)channels);
  printf("  format: ");
  spike_print_status(result->formatStatus);
  printf("\n  layout: ");
  spike_print_status(result->layoutStatus);
  printf("\n  initialize: ");
  spike_print_status(result->initializeStatus);
  printf("\n  render: ");
  spike_print_status(result->renderStatus);
  printf("\n  latency: %.9f s; first=%u; peak=%u; magnitude=%.9g\n",
         result->reportedLatencySeconds,
         (unsigned int)result->firstNonzeroFrame,
         (unsigned int)result->peakFrame, result->peakMagnitude);
  printf("  input callbacks: %u/%u buses seen\n",
         (unsigned int)result->callbacksSeen,
         (unsigned int)result->inputBusCount);
  printf("  policy readback: ");
  spike_print_status(result->policyReadbackStatus);
  printf("; reverb=%u; flags=0x%x/0x%x; blend=%.1f/%.1f; "
         "occlusion=%.1f/%.1f; obstruction=%.1f/%.1f\n",
         (unsigned int)result->internalReverb,
         (unsigned int)result->objectRenderingFlags,
         (unsigned int)result->environmentRenderingFlags,
         result->objectReverbBlend, result->environmentReverbBlend,
         result->objectOcclusion, result->environmentOcclusion,
         result->objectObstruction, result->environmentObstruction);
  printf("  head-tracking setter: ");
  spike_print_status(result->headTrackingSetStatus);
  printf("; personalized-mode setter: ");
  spike_print_status(result->personalizedModeSetStatus);
  printf("; personalized-active query: ");
  spike_print_status(result->personalizedActiveGetStatus);
  printf("; active=%u\n", (unsigned int)result->personalizedActive);
}

static int spike_result_passed(const SpikeProbeResult *result) {
  return result->formatStatus == noErr && result->layoutStatus == noErr &&
         result->initializeStatus == noErr && result->renderStatus == noErr &&
         result->firstNonzeroFrame != UINT32_MAX &&
         result->callbacksSeen == result->inputBusCount &&
         result->policyReadbackStatus == noErr && result->internalReverb == 0 &&
         result->objectRenderingFlags ==
             kSpatialMixerRenderingFlags_InterAuralDelay &&
         result->environmentRenderingFlags ==
             kSpatialMixerRenderingFlags_InterAuralDelay &&
         result->objectReverbBlend == 0.0f &&
         result->environmentReverbBlend == 0.0f &&
         result->objectOcclusion == 0.0f &&
         result->environmentOcclusion == 0.0f &&
         result->objectObstruction == 0.0f &&
         result->environmentObstruction == 0.0f;
}

static Float32 spike_n3d_to_sn3d_gain(UInt32 acn) {
  UInt32 order = 0;
  while ((order + 1) * (order + 1) <= acn) {
    ++order;
  }
  return 1.0f / sqrtf((Float32)(2 * order + 1));
}

static int spike_ratio_matches(const SpikeProbeResult *sn3d,
                               const SpikeProbeResult *n3d, UInt32 acn) {
  if (sn3d->peakMagnitude <= 0.0f || n3d->peakMagnitude <= 0.0f) {
    return 0;
  }
  Float32 measured = n3d->peakMagnitude / sn3d->peakMagnitude;
  return fabsf(measured - spike_n3d_to_sn3d_gain(acn)) <= 1.0e-3f;
}

_Static_assert(kAudioChannelLabel_HOA_ACN_0 == ((2U << 16) | 0),
               "Apple HOA SN3D channels must begin at ACN 0");
_Static_assert(
    kAudioChannelLabel_HOA_ACN_8 == ((2U << 16) | 8),
    "Apple order-2 HOA channels must remain contiguous through ACN 8");

int main(void) {
  const AudioChannelLayoutTag sn3dOrder0 =
      kAudioChannelLayoutTag_HOA_ACN_SN3D | 1;
  const AudioChannelLayoutTag sn3dFoa = kAudioChannelLayoutTag_HOA_ACN_SN3D | 4;
  const AudioChannelLayoutTag sn3dOrder2 =
      kAudioChannelLayoutTag_HOA_ACN_SN3D | 9;
  const AudioChannelLayoutTag n3dFoa = kAudioChannelLayoutTag_HOA_ACN_N3D | 4;
  const AudioChannelLayoutTag n3dOrder2 =
      kAudioChannelLayoutTag_HOA_ACN_N3D | 9;

  SpikeProbeResult sn3dOrder0Result = spike_probe(sn3dOrder0, 1, 1, 1, 0);
  SpikeProbeResult monoWithSn3dFoa = spike_probe(sn3dFoa, 4, 1, 0, 0);
  SpikeProbeResult sn3dFoaResult = spike_probe(sn3dFoa, 4, 1, 1, 0);
  SpikeProbeResult sn3dFoaAcn1Result = spike_probe(sn3dFoa, 4, 1, 1, 1);
  SpikeProbeResult monoWithSn3dOrder2 = spike_probe(sn3dOrder2, 9, 1, 0, 0);
  SpikeProbeResult sn3dOrder2Result = spike_probe(sn3dOrder2, 9, 1, 1, 0);
  SpikeProbeResult sn3dOrder2Acn8Result = spike_probe(sn3dOrder2, 9, 1, 1, 8);
  SpikeProbeResult structuralMaximumResult = spike_probe(
      sn3dOrder2, 9, kSpikeMaximumObjectBuses, kSpikeMaximumObjectBuses, 0);
  SpikeProbeResult n3dFoaResult = spike_probe(n3dFoa, 4, 1, 1, 0);
  SpikeProbeResult n3dFoaAcn1Result = spike_probe(n3dFoa, 4, 1, 1, 1);
  SpikeProbeResult n3dOrder2Result = spike_probe(n3dOrder2, 9, 1, 1, 0);
  SpikeProbeResult n3dOrder2Acn8Result = spike_probe(n3dOrder2, 9, 1, 1, 8);
  SpikeProbeResult legacyBFormatResult =
      spike_probe(kAudioChannelLayoutTag_Ambisonic_B_Format, 4, 1, 1, 0);

  spike_print_result("mono + SN3D order 0", sn3dOrder0, 1, &sn3dOrder0Result);
  spike_print_result("mono + SN3D FOA (mono impulse)", sn3dFoa, 4,
                     &monoWithSn3dFoa);
  spike_print_result("mono + SN3D FOA (field impulse)", sn3dFoa, 4,
                     &sn3dFoaResult);
  spike_print_result("mono + SN3D FOA (ACN 1 impulse)", sn3dFoa, 4,
                     &sn3dFoaAcn1Result);
  spike_print_result("mono + SN3D order 2 (mono impulse)", sn3dOrder2, 9,
                     &monoWithSn3dOrder2);
  spike_print_result("mono + SN3D order 2", sn3dOrder2, 9, &sn3dOrder2Result);
  spike_print_result("mono + SN3D order 2 (ACN 8 impulse)", sn3dOrder2, 9,
                     &sn3dOrder2Acn8Result);
  spike_print_result("48 mono + SN3D order 2 (field impulse)", sn3dOrder2, 9,
                     &structuralMaximumResult);
  spike_print_result("mono + N3D FOA", n3dFoa, 4, &n3dFoaResult);
  spike_print_result("mono + N3D FOA (ACN 1 impulse)", n3dFoa, 4,
                     &n3dFoaAcn1Result);
  spike_print_result("mono + N3D order 2", n3dOrder2, 9, &n3dOrder2Result);
  spike_print_result("mono + N3D order 2 (ACN 8 impulse)", n3dOrder2, 9,
                     &n3dOrder2Acn8Result);
  spike_print_result("mono + legacy B-format FOA (WXYZ)",
                     kAudioChannelLayoutTag_Ambisonic_B_Format, 4,
                     &legacyBFormatResult);

  Float32 foaNormalizationRatio =
      n3dFoaAcn1Result.peakMagnitude / sn3dFoaAcn1Result.peakMagnitude;
  Float32 order2NormalizationRatio =
      n3dOrder2Acn8Result.peakMagnitude / sn3dOrder2Acn8Result.peakMagnitude;
  int foaPeakDelta =
      (int)sn3dFoaResult.peakFrame - (int)monoWithSn3dFoa.peakFrame;
  int order2PeakDelta =
      (int)sn3dOrder2Result.peakFrame - (int)monoWithSn3dOrder2.peakFrame;
  printf("summary: N3D/SN3D ACN1 ratio=%.9f expected=%.9f; "
         "ACN8 ratio=%.9f expected=%.9f\n",
         foaNormalizationRatio, spike_n3d_to_sn3d_gain(1),
         order2NormalizationRatio, spike_n3d_to_sn3d_gain(8));
  printf("summary: object/environment peak delta: FOA=%d frame(s), "
         "order2=%d frame(s)\n",
         foaPeakDelta, order2PeakDelta);

  int advancedPropertySettersPassed = 1;
  if (__builtin_available(macOS 13.0, iOS 18.0, tvOS 18.0, *)) {
    advancedPropertySettersPassed =
        structuralMaximumResult.headTrackingSetStatus == noErr &&
        structuralMaximumResult.personalizedModeSetStatus == noErr;
  }

  return spike_result_passed(&sn3dOrder0Result) &&
                 spike_result_passed(&monoWithSn3dFoa) &&
                 spike_result_passed(&sn3dFoaResult) &&
                 spike_result_passed(&sn3dFoaAcn1Result) &&
                 spike_result_passed(&monoWithSn3dOrder2) &&
                 spike_result_passed(&sn3dOrder2Result) &&
                 spike_result_passed(&sn3dOrder2Acn8Result) &&
                 spike_result_passed(&structuralMaximumResult) &&
                 spike_result_passed(&n3dFoaResult) &&
                 spike_result_passed(&n3dFoaAcn1Result) &&
                 spike_result_passed(&n3dOrder2Result) &&
                 spike_result_passed(&n3dOrder2Acn8Result) &&
                 spike_ratio_matches(&sn3dFoaAcn1Result, &n3dFoaAcn1Result,
                                     1) &&
                 spike_ratio_matches(&sn3dOrder2Acn8Result,
                                     &n3dOrder2Acn8Result, 8) &&
                 abs(foaPeakDelta) <= 1 && abs(order2PeakDelta) <= 1 &&
                 advancedPropertySettersPassed
             ? EXIT_SUCCESS
             : EXIT_FAILURE;
}
