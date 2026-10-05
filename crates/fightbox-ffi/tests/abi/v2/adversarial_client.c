#include "fightbox.h"
#include "abi_declaration_contract.h"
#include "abi_layout_contract.h"

#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

/* Labels and exit codes are preserved gate-log diagnostics. Each failure site
 * owns one unique code in 1..125; code 125 is reserved for the self-test. */
static int contract_failure(int exit_code, const char *contract_label) {
  (void)fprintf(stderr,
                "fightbox C ABI v2 contract: FAIL contract=%s exit_code=%d\n",
                contract_label, exit_code);
  return exit_code;
}

#define REQUIRE_CONTRACT(exit_code, contract_label, condition)                 \
  do {                                                                         \
    if (!(condition)) {                                                        \
      return contract_failure((exit_code), (contract_label));                  \
    }                                                                          \
  } while (0)

#define REQUIRE_SESSION_CONTRACT(exit_code, contract_label, session,          \
                                 condition)                                    \
  do {                                                                         \
    if (!(condition)) {                                                        \
      (void)fb_session_destroy((session));                                      \
      return contract_failure((exit_code), (contract_label));                  \
    }                                                                          \
  } while (0)

#ifdef FB_V2_ADVERSARIAL_DIAGNOSTIC_SELF_TEST
int main(void) {
  return contract_failure(125, "diagnostic.self-test");
}
#else

static const FbCreateV2Signature kCreateV2 = &fb_session_create_v2;
static const FbConfigureSourceV2Signature kConfigureSourceV2 =
    &fb_session_configure_source_v2;
static const FbUpdateControlFrameV2Signature kUpdateControlFrameV2 =
    &fb_session_update_control_frame_v2;
static const FbPrepareSpatialV2Signature kPrepareSpatialV2 =
    &fb_session_prepare_spatial_v2;
static const FbRenderSpatialV2Signature kRenderSpatialV2 =
    &fb_session_render_spatial_v2;

struct ShortHeaderV2 {
  uint32_t abi_version;
  uint32_t struct_size;
};

struct ConfigGuard {
  uint64_t before;
  struct FbSessionConfigV2 value;
  uint64_t after;
};

struct SessionOutGuard {
  uint64_t before;
  struct FbSession *value;
  uint64_t after;
};

struct DirectGuard {
  uint64_t before;
  float samples[FB_MAX_PRESENTATION_FEEDS_V2 * 4];
  uint64_t after;
};

struct EnvironmentalGuard {
  uint64_t before;
  float samples[FB_MAX_ENVIRONMENTAL_CHANNELS_V2 * 4];
  uint64_t after;
};

struct FeedGuard {
  uint64_t before;
  struct FbPresentationFeedMetadataV2
      values[FB_MAX_PRESENTATION_FEEDS_V2];
  uint64_t after;
};

struct MetadataGuard {
  uint64_t before;
  struct FbSpatialBlockMetadataV2 value;
  uint64_t after;
};

struct BlockGuard {
  uint64_t before;
  struct FbSpatialRenderBlockV2 value;
  uint64_t after;
};

struct SourceUpdatesGuard {
  uint64_t before;
  struct FbSourceUpdate values[2];
  uint64_t after;
};

struct ControlFrameGuard {
  uint64_t before;
  struct FbControlFrameV2 value;
  uint64_t after;
};

static struct FbSessionConfigV2 neutral_config(uint32_t source_count) {
  const struct FbSessionConfigV2 value = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct FbSessionConfigV2),
      .sample_rate_hz = 48000,
      .block_size_frames = 4,
      .source_count = source_count,
      .default_source_level_db = 0.0f,
      .quality_tier = FbQualityDesktop,
      .render_route = FbRenderNeutralSpatialV2,
      .environmental_order = 2,
      .reserved = {0},
  };
  return value;
}

static struct FbSourceProgramConfigV2 point_config(uint32_t source_index) {
  const struct FbSourceProgramConfigV2 value = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct FbSourceProgramConfigV2),
      .source_index = source_index,
      .channel_count = 1,
      .source_geometry = FbSourceGeometryPointV2,
      .multipoint_count = 0,
      .extent_m = 0.0f,
      .reserved_f32 = 0.0f,
      .reserved = {0},
  };
  return value;
}

static struct FbPose pose_at(float east_m, float north_m, float up_m) {
  const struct FbPose value = {
      .position = {east_m, north_m, up_m},
      .forward = {0.0f, 1.0f, 0.0f},
      .up = {0.0f, 0.0f, 1.0f},
  };
  return value;
}

static struct FbSourceUpdate source_update_at(float east_m, float north_m,
                                              float up_m) {
  const struct FbSourceUpdate value = {
      .active = 1,
      .pose = {
          .position = {east_m, north_m, up_m},
          .forward = {0.0f, 1.0f, 0.0f},
          .up = {0.0f, 0.0f, 1.0f},
      },
      .linear_velocity_mps = {0.0f, 0.0f, 0.0f},
  };
  return value;
}

static struct FbControlFrameV2 control_frame(
    const struct FbSourceUpdate *source_updates, uint32_t source_count,
    struct FbPose listener_pose) {
  const struct FbControlFrameV2 value = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct FbControlFrameV2),
      .source_updates = source_updates,
      .source_count = source_count,
      .source_update_stride_bytes = sizeof(struct FbSourceUpdate),
      .listener_pose = listener_pose,
      .listener_linear_velocity_mps = {0.0f, 0.0f, 0.0f},
      .reserved = {0},
  };
  return value;
}

static int samples_equal(const float *samples, size_t count, float expected) {
  for (size_t index = 0; index < count; ++index) {
    if (samples[index] != expected) {
      return 0;
    }
  }
  return 1;
}

static int samples_finite(const float *samples, size_t count) {
  for (size_t index = 0; index < count; ++index) {
    if (!isfinite(samples[index])) {
      return 0;
    }
  }
  return 1;
}

static int float_near(float actual, float expected) {
  return fabsf(actual - expected) <= 1.0e-6f;
}

static int validate_create_rejections(void) {
  const char valid_path[] = "syntactically-valid";
  struct ConfigGuard config = {
      .before = UINT64_C(0x1122334455667788),
      .value = {0},
      .after = UINT64_C(0x8877665544332211),
  };
  struct SessionOutGuard output = {
      .before = UINT64_C(0xA1A2A3A4A5A6A7A8),
      .value = (struct FbSession *)(uintptr_t)1,
      .after = UINT64_C(0xB1B2B3B4B5B6B7B8),
  };
  config.value = neutral_config(1);

  _Alignas(8) const struct ShortHeaderV2 short_header = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct ShortHeaderV2),
  };
  enum FbResult result = fb_session_create_v2(
      (const struct FbSessionConfigV2 *)(const void *)&short_header, valid_path,
      valid_path, &output.value);
  REQUIRE_CONTRACT(20, "create.short-config.result-invalid-argument",
                   result == FbInvalidArgument);
  REQUIRE_CONTRACT(4, "create.short-config.output-cleared",
                   output.value == NULL);

  config.value.abi_version = 1;
  output.value = (struct FbSession *)(uintptr_t)1;
  result = fb_session_create_v2(&config.value, NULL, NULL, &output.value);
  REQUIRE_CONTRACT(21, "create.v1-version.result-invalid-argument",
                   result == FbInvalidArgument);
  REQUIRE_CONTRACT(5, "create.v1-version.output-cleared",
                   output.value == NULL);

  config.value = neutral_config(1);
  config.value.reserved[6] = 1;
  output.value = (struct FbSession *)(uintptr_t)1;
  result = fb_session_create_v2(&config.value, NULL, NULL, &output.value);
  REQUIRE_CONTRACT(22, "create.reserved-field.result-invalid-argument",
                   result == FbInvalidArgument);
  REQUIRE_CONTRACT(6, "create.reserved-field.output-cleared",
                   output.value == NULL);

  config.value = neutral_config(1);
  output.value = (struct FbSession *)(uintptr_t)1;
  result =
      fb_session_create_v2(&config.value, "", valid_path, &output.value);
  REQUIRE_CONTRACT(23, "create.empty-package-path.result-invalid-argument",
                   result == FbInvalidArgument);
  REQUIRE_CONTRACT(7, "create.empty-package-path.output-cleared",
                   output.value == NULL);

  REQUIRE_CONTRACT(24, "create.null-arguments.result-invalid-argument",
                   fb_session_create_v2(NULL, NULL, NULL, NULL) ==
                       FbInvalidArgument);
  REQUIRE_CONTRACT(25, "create.guard.config-before-intact",
                   config.before == UINT64_C(0x1122334455667788));
  REQUIRE_CONTRACT(8, "create.guard.config-after-intact",
                   config.after == UINT64_C(0x8877665544332211));
  REQUIRE_CONTRACT(9, "create.guard.output-before-intact",
                   output.before == UINT64_C(0xA1A2A3A4A5A6A7A8));
  REQUIRE_CONTRACT(12, "create.guard.output-after-intact",
                   output.after == UINT64_C(0xB1B2B3B4B5B6B7B8));
  return 0;
}

static int run_neutral_binding(const char *package_path, const char *bake_path) {
  struct ConfigGuard config = {
      .before = UINT64_C(0x1020304050607080),
      .value = {0},
      .after = UINT64_C(0x8070605040302010),
  };
  struct SessionOutGuard output = {
      .before = UINT64_C(0x0102030405060708),
      .value = NULL,
      .after = UINT64_C(0x0807060504030201),
  };
  config.value = neutral_config(2);
  enum FbResult result = fb_session_create_v2(
      &config.value, package_path, bake_path, &output.value);
  REQUIRE_CONTRACT(30, "lifecycle.create-neutral.result-ok", result == FbOk);
  REQUIRE_CONTRACT(13, "lifecycle.create-neutral.session-nonnull",
                   output.value != NULL);
  struct FbSession *session = output.value;

  REQUIRE_SESSION_CONTRACT(
      31, "lifecycle.render-before-configuration.invalid-state", session,
      fb_session_render_spatial_v2(session, NULL) == FbInvalidState);
  REQUIRE_SESSION_CONTRACT(
      60, "lifecycle.prepare-before-configuration.invalid-state", session,
      fb_session_prepare_spatial_v2(session) == FbInvalidState);

  _Alignas(8) const struct ShortHeaderV2 short_source = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct ShortHeaderV2),
  };
  REQUIRE_SESSION_CONTRACT(
      32, "configure.short-source.result-invalid-argument", session,
      fb_session_configure_source_v2(
          session,
          (const struct FbSourceProgramConfigV2 *)(const void *)&short_source) ==
          FbInvalidArgument);

  const struct FbSourceProgramConfigV2 source_zero = point_config(0);
  REQUIRE_SESSION_CONTRACT(
      33, "configure.source-zero.result-ok", session,
      fb_session_configure_source_v2(session, &source_zero) == FbOk);
  REQUIRE_SESSION_CONTRACT(
      14, "lifecycle.render-before-all-sources.invalid-state", session,
      fb_session_render_spatial_v2(session, NULL) == FbInvalidState);
  REQUIRE_SESSION_CONTRACT(
      61, "lifecycle.prepare-after-source-zero.invalid-state", session,
      fb_session_prepare_spatial_v2(session) == FbInvalidState);
  const struct FbSourceProgramConfigV2 source_one = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct FbSourceProgramConfigV2),
      .source_index = 1,
      .channel_count = 2,
      .source_geometry = FbSourceGeometryStereoImageV2,
      .multipoint_count = 0,
      .extent_m = 2.0f,
      .reserved_f32 = 0.0f,
      .reserved = {0},
  };
  REQUIRE_SESSION_CONTRACT(
      34, "configure.source-one.result-ok", session,
      fb_session_configure_source_v2(session, &source_one) == FbOk);
  REQUIRE_SESSION_CONTRACT(
      62, "lifecycle.prepare-before-listener.invalid-state", session,
      fb_session_prepare_spatial_v2(session) == FbInvalidState);

  struct SourceUpdatesGuard source_updates = {
      .before = UINT64_C(0x2468ACE013579BDF),
      .values = {{0}},
      .after = UINT64_C(0xFDB975310ECA8642),
  };
  source_updates.values[0] = source_update_at(1.0f, 2.0f, 0.0f);
  source_updates.values[1] = source_update_at(2.0f, 2.0f, 0.0f);
  struct ControlFrameGuard control_frame_guard = {
      .before = UINT64_C(0xAABBCCDDEEFF0011),
      .value = {0},
      .after = UINT64_C(0x1100FFEEDDCCBBAA),
  };
  control_frame_guard.value =
      control_frame(source_updates.values, 2, pose_at(0.0f, 0.0f, 0.0f));

  _Alignas(8) const struct ShortHeaderV2 short_control_frame = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct ShortHeaderV2),
  };
  REQUIRE_SESSION_CONTRACT(
      117, "control-frame.short-outer.result-invalid-argument", session,
      fb_session_update_control_frame_v2(
          session,
          (const struct FbControlFrameV2 *)(const void *)&short_control_frame) ==
          FbInvalidArgument);

  control_frame_guard.value.abi_version = 1;
  REQUIRE_SESSION_CONTRACT(
      118, "control-frame.v1-version.result-invalid-argument", session,
      fb_session_update_control_frame_v2(session, &control_frame_guard.value) ==
          FbInvalidArgument);
  control_frame_guard.value.abi_version = FB_ABI_VERSION_V2;

  control_frame_guard.value.reserved[3] = 1;
  REQUIRE_SESSION_CONTRACT(
      119, "control-frame.reserved-field.result-invalid-argument", session,
      fb_session_update_control_frame_v2(session, &control_frame_guard.value) ==
          FbInvalidArgument);
  control_frame_guard.value.reserved[3] = 0;

  control_frame_guard.value.source_count = 1;
  REQUIRE_SESSION_CONTRACT(
      120, "control-frame.source-count-mismatch.result-invalid-argument",
      session,
      fb_session_update_control_frame_v2(session, &control_frame_guard.value) ==
          FbInvalidArgument);
  control_frame_guard.value.source_count = 2;

  control_frame_guard.value.source_update_stride_bytes =
      sizeof(struct FbSourceUpdate) - 1;
  REQUIRE_SESSION_CONTRACT(
      121, "control-frame.short-source-stride.result-invalid-argument",
      session,
      fb_session_update_control_frame_v2(session, &control_frame_guard.value) ==
          FbInvalidArgument);
  control_frame_guard.value.source_update_stride_bytes =
      sizeof(struct FbSourceUpdate);

  control_frame_guard.value.source_updates = NULL;
  REQUIRE_SESSION_CONTRACT(
      122, "control-frame.null-source-array.result-invalid-argument", session,
      fb_session_update_control_frame_v2(session, &control_frame_guard.value) ==
          FbInvalidArgument);
  control_frame_guard.value.source_updates = source_updates.values;

  control_frame_guard.value.listener_pose.position.east_m = NAN;
  REQUIRE_SESSION_CONTRACT(
      123, "control-frame.invalid-listener.result-invalid-argument", session,
      fb_session_update_control_frame_v2(session, &control_frame_guard.value) ==
          FbInvalidArgument);
  control_frame_guard.value.listener_pose.position.east_m = 0.0f;

  source_updates.values[1].linear_velocity_mps.north_m = INFINITY;
  REQUIRE_SESSION_CONTRACT(
      124, "control-frame.invalid-last-source.result-invalid-argument", session,
      fb_session_update_control_frame_v2(session, &control_frame_guard.value) ==
          FbInvalidArgument);
  source_updates.values[1].linear_velocity_mps.north_m = 0.0f;

  REQUIRE_SESSION_CONTRACT(
      63, "control-frame.rejections-leave-session-unprepared", session,
      fb_session_prepare_spatial_v2(session) == FbInvalidState);
  REQUIRE_SESSION_CONTRACT(
      64, "control-frame.valid-neutral.result-ok", session,
      fb_session_update_control_frame_v2(session, &control_frame_guard.value) ==
          FbOk);
  REQUIRE_SESSION_CONTRACT(
      65, "control-frame.input-guard-canaries-intact", session,
      source_updates.before == UINT64_C(0x2468ACE013579BDF) &&
          source_updates.after == UINT64_C(0xFDB975310ECA8642) &&
          control_frame_guard.before == UINT64_C(0xAABBCCDDEEFF0011) &&
          control_frame_guard.after == UINT64_C(0x1100FFEEDDCCBBAA));

  _Alignas(8) const struct ShortHeaderV2 short_block = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct ShortHeaderV2),
  };
  REQUIRE_SESSION_CONTRACT(
      67, "lifecycle.short-block-before-prepare.invalid-state", session,
      fb_session_render_spatial_v2(
          session,
          (const struct FbSpatialRenderBlockV2 *)(const void *)&short_block) ==
          FbInvalidState);
  REQUIRE_SESSION_CONTRACT(68, "lifecycle.prepare-complete.result-ok", session,
                           fb_session_prepare_spatial_v2(session) == FbOk);
  REQUIRE_SESSION_CONTRACT(
      35, "render.short-block-after-prepare.invalid-argument", session,
      fb_session_render_spatial_v2(
          session,
          (const struct FbSpatialRenderBlockV2 *)(const void *)&short_block) ==
          FbInvalidArgument);

  const float mono[4] = {0.25f, 0.25f, 0.25f, 0.25f};
  const float stereo[8] = {0.5f, 0.5f, 0.5f, 0.5f,
                           0.75f, 0.75f, 0.75f, 0.75f};
  const struct FbSourceProgramInputV2 programs[2] = {
      {
          .source_index = 0,
          .channel_count = 1,
          .samples = mono,
          .sample_count = 4,
          .channel_stride_samples = 4,
          .reserved = {0},
      },
      {
          .source_index = 1,
          .channel_count = 2,
          .samples = stereo,
          .sample_count = 8,
          .channel_stride_samples = 4,
          .reserved = {0},
      },
  };
  struct DirectGuard direct = {
      .before = UINT64_C(0x1111222233334444),
      .samples = {0},
      .after = UINT64_C(0x4444333322221111),
  };
  struct EnvironmentalGuard environmental = {
      .before = UINT64_C(0x5555666677778888),
      .samples = {0},
      .after = UINT64_C(0x8888777766665555),
  };
  for (size_t index = 0;
       index < sizeof(direct.samples) / sizeof(direct.samples[0]); ++index) {
    direct.samples[index] = 7.0f;
  }
  for (size_t index = 0;
       index < sizeof(environmental.samples) / sizeof(environmental.samples[0]);
       ++index) {
    environmental.samples[index] = 11.0f;
  }
  struct FeedGuard feeds = {
      .before = UINT64_C(0x9999AAAABBBBCCCC),
      .values = {{0}},
      .after = UINT64_C(0xCCCCBBBBAAAA9999),
  };
  struct MetadataGuard metadata = {
      .before = UINT64_C(0xDDDDEEEEFFFF0000),
      .value = {
          .abi_version = FB_ABI_VERSION_V2,
          .struct_size = sizeof(struct FbSpatialBlockMetadataV2),
      },
      .after = UINT64_C(0x0000FFFFEEEEDDDD),
  };
  struct BlockGuard block = {
      .before = UINT64_C(0x13579BDF2468ACE0),
      .value = {
          .abi_version = FB_ABI_VERSION_V2,
          .struct_size = sizeof(struct FbSpatialRenderBlockV2),
          .source_programs = programs,
          .source_program_count = 2,
          .source_program_stride_bytes =
              sizeof(struct FbSourceProgramInputV2),
          .direct_output =
              {
                  .samples = direct.samples,
                  .sample_capacity = sizeof(direct.samples) / sizeof(float),
                  .plane_capacity = FB_MAX_PRESENTATION_FEEDS_V2,
                  .reserved_u32 = 0,
                  .plane_stride_samples = 4,
                  .reserved = {0},
              },
          .environmental_output =
              {
                  .samples = environmental.samples,
                  .sample_capacity =
                      sizeof(environmental.samples) / sizeof(float),
                  .plane_capacity = FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                  .reserved_u32 = 0,
                  .plane_stride_samples = 4,
                  .reserved = {0},
              },
          .feed_metadata = feeds.values,
          .feed_metadata_capacity = FB_MAX_PRESENTATION_FEEDS_V2,
          .feed_metadata_stride_bytes =
              sizeof(struct FbPresentationFeedMetadataV2),
          .block_metadata = &metadata.value,
          .reserved = {0},
      },
      .after = UINT64_C(0x0ECA8642FDB97531),
  };

  REQUIRE_SESSION_CONTRACT(
      36, "render.valid-block.result-ok", session,
      fb_session_render_spatial_v2(session, &block.value) == FbOk);
  const uint32_t expected_flags =
      FB_SPATIAL_BLOCK_VALID_V2 | FB_SPATIAL_SOURCE_SAFETY_APPLIED_V2 |
      FB_SPATIAL_OUTPUT_LIMITER_UNAPPLIED_V2 |
      FB_SPATIAL_WORLD_UNROTATED_V2 |
      FB_SPATIAL_FINAL_HRTF_UNAPPLIED_V2 |
      FB_SPATIAL_SOURCE_DRIVE_APPLIED_V2 |
      FB_SPATIAL_MONITOR_GAIN_UNAPPLIED_V2;
  REQUIRE_SESSION_CONTRACT(
      37, "render.samples.direct.finite", session,
      samples_finite(direct.samples,
                     sizeof(direct.samples) / sizeof(direct.samples[0])));
  REQUIRE_SESSION_CONTRACT(
      15, "render.samples.environmental.finite", session,
      samples_finite(environmental.samples,
                     sizeof(environmental.samples) /
                         sizeof(environmental.samples[0])));
  REQUIRE_SESSION_CONTRACT(
      16, "render.samples.direct.overwritten", session,
      !samples_equal(direct.samples,
                     sizeof(direct.samples) / sizeof(direct.samples[0]), 7.0f));
  REQUIRE_SESSION_CONTRACT(
      17, "render.samples.environmental.overwritten", session,
      !samples_equal(environmental.samples,
                     sizeof(environmental.samples) /
                         sizeof(environmental.samples[0]),
                     11.0f));
  REQUIRE_SESSION_CONTRACT(18, "render.metadata.validity.valid", session,
                           metadata.value.validity == FbSpatialValidV2);
  REQUIRE_SESSION_CONTRACT(19, "render.metadata.block-start-frame.initial",
                           session, metadata.value.block_start_frame == 0);
  REQUIRE_SESSION_CONTRACT(
      26, "render.metadata.active-feed-count", session,
      metadata.value.active_presentation_feed_count == 3);
  REQUIRE_SESSION_CONTRACT(27, "render.metadata.environmental-order", session,
                           metadata.value.environmental_order == 2);
  REQUIRE_SESSION_CONTRACT(
      28, "render.metadata.environmental-channel-count", session,
      metadata.value.environmental_channel_count == 9);
  REQUIRE_SESSION_CONTRACT(
      29, "render.metadata.component-mask", session,
      metadata.value.component_mask ==
          (FB_PRESENTATION_COMPONENT_DIRECT_CENTER_V2 |
           FB_PRESENTATION_COMPONENT_WIDTH_POSITIVE_V2 |
           FB_PRESENTATION_COMPONENT_WIDTH_NEGATIVE_V2));
  REQUIRE_SESSION_CONTRACT(41, "render.metadata.flags", session,
                           metadata.value.flags == expected_flags);
  REQUIRE_SESSION_CONTRACT(
      42, "render.metadata.environmental-channel-order", session,
      metadata.value.environmental_channel_order == FbEnvironmentalAcnV2);
  REQUIRE_SESSION_CONTRACT(
      43, "render.metadata.environmental-normalization", session,
      metadata.value.environmental_normalization == FbEnvironmentalN3dV2);
  REQUIRE_SESSION_CONTRACT(
      44, "render.metadata.environmental-basis", session,
      metadata.value.environmental_basis ==
          FbEnvironmentalSteamXRightYUpZBackV2);

  REQUIRE_SESSION_CONTRACT(45, "render.feed-0.valid", session,
                           feeds.values[0].valid == 1);
  REQUIRE_SESSION_CONTRACT(46, "render.feed-0.source-index", session,
                           feeds.values[0].source_index == 0);
  REQUIRE_SESSION_CONTRACT(
      47, "render.feed-0.component", session,
      feeds.values[0].component == FB_PRESENTATION_COMPONENT_DIRECT_CENTER_V2);
  REQUIRE_SESSION_CONTRACT(48, "render.feed-0.placement", session,
                           feeds.values[0].placement ==
                               FbPresentationDirectionV2);
  REQUIRE_SESSION_CONTRACT(49, "render.feed-0.pose.east", session,
                           feeds.values[0].pose.position.east_m == 1.0f);
  REQUIRE_SESSION_CONTRACT(53, "render.feed-0.pose.north", session,
                           feeds.values[0].pose.position.north_m == 2.0f);
  REQUIRE_SESSION_CONTRACT(54, "render.feed-0.pose.up", session,
                           feeds.values[0].pose.position.up_m == 0.0f);
  REQUIRE_SESSION_CONTRACT(
      55, "render.feed-0.direction.east", session,
      float_near(feeds.values[0].direction_enu.east_m, 0.4472136f));
  REQUIRE_SESSION_CONTRACT(
      56, "render.feed-0.direction.north", session,
      float_near(feeds.values[0].direction_enu.north_m, 0.8944272f));
  REQUIRE_SESSION_CONTRACT(57, "render.feed-0.direction.up", session,
                           feeds.values[0].direction_enu.up_m == 0.0f);
  REQUIRE_SESSION_CONTRACT(
      58, "render.feed-0.processing-latency-frames", session,
      feeds.values[0].processing_latency_frames == 0);
  REQUIRE_SESSION_CONTRACT(59, "render.feed-3.invalid", session,
                           feeds.values[3].valid == 0);

  REQUIRE_SESSION_CONTRACT(69, "render.feed-4.valid", session,
                           feeds.values[4].valid == 1);
  REQUIRE_SESSION_CONTRACT(70, "render.feed-4.source-index", session,
                           feeds.values[4].source_index == 1);
  REQUIRE_SESSION_CONTRACT(
      71, "render.feed-4.component", session,
      feeds.values[4].component ==
          FB_PRESENTATION_COMPONENT_WIDTH_POSITIVE_V2);
  REQUIRE_SESSION_CONTRACT(72, "render.feed-4.placement", session,
                           feeds.values[4].placement ==
                               FbPresentationDirectionV2);
  REQUIRE_SESSION_CONTRACT(73, "render.feed-4.pose.east", session,
                           feeds.values[4].pose.position.east_m == 3.0f);
  REQUIRE_SESSION_CONTRACT(74, "render.feed-4.pose.north", session,
                           feeds.values[4].pose.position.north_m == 2.0f);
  REQUIRE_SESSION_CONTRACT(
      75, "render.feed-4.direction.east", session,
      float_near(feeds.values[4].direction_enu.east_m, 0.8320503f));
  REQUIRE_SESSION_CONTRACT(
      76, "render.feed-4.direction.north", session,
      float_near(feeds.values[4].direction_enu.north_m, 0.5547002f));
  REQUIRE_SESSION_CONTRACT(77, "render.feed-4.direction.up", session,
                           feeds.values[4].direction_enu.up_m == 0.0f);

  REQUIRE_SESSION_CONTRACT(78, "render.feed-5.valid", session,
                           feeds.values[5].valid == 1);
  REQUIRE_SESSION_CONTRACT(79, "render.feed-5.source-index", session,
                           feeds.values[5].source_index == 1);
  REQUIRE_SESSION_CONTRACT(
      82, "render.feed-5.component", session,
      feeds.values[5].component ==
          FB_PRESENTATION_COMPONENT_WIDTH_NEGATIVE_V2);
  REQUIRE_SESSION_CONTRACT(83, "render.feed-5.placement", session,
                           feeds.values[5].placement ==
                               FbPresentationDirectionV2);
  REQUIRE_SESSION_CONTRACT(84, "render.feed-5.pose.east", session,
                           feeds.values[5].pose.position.east_m == 1.0f);
  REQUIRE_SESSION_CONTRACT(85, "render.feed-5.pose.north", session,
                           feeds.values[5].pose.position.north_m == 2.0f);
  REQUIRE_SESSION_CONTRACT(
      86, "render.feed-5.direction.east", session,
      float_near(feeds.values[5].direction_enu.east_m, 0.4472136f));
  REQUIRE_SESSION_CONTRACT(
      87, "render.feed-5.direction.north", session,
      float_near(feeds.values[5].direction_enu.north_m, 0.8944272f));
  REQUIRE_SESSION_CONTRACT(88, "render.feed-5.direction.up", session,
                           feeds.values[5].direction_enu.up_m == 0.0f);

  REQUIRE_SESSION_CONTRACT(89, "render.guard.direct-before-intact", session,
                           direct.before == UINT64_C(0x1111222233334444));
  REQUIRE_SESSION_CONTRACT(90, "render.guard.direct-after-intact", session,
                           direct.after == UINT64_C(0x4444333322221111));
  REQUIRE_SESSION_CONTRACT(
      91, "render.guard.environmental-before-intact", session,
      environmental.before == UINT64_C(0x5555666677778888));
  REQUIRE_SESSION_CONTRACT(
      92, "render.guard.environmental-after-intact", session,
      environmental.after == UINT64_C(0x8888777766665555));
  REQUIRE_SESSION_CONTRACT(93, "render.guard.feeds-before-intact", session,
                           feeds.before == UINT64_C(0x9999AAAABBBBCCCC));
  REQUIRE_SESSION_CONTRACT(94, "render.guard.feeds-after-intact", session,
                           feeds.after == UINT64_C(0xCCCCBBBBAAAA9999));
  REQUIRE_SESSION_CONTRACT(
      95, "render.guard.metadata-before-intact", session,
      metadata.before == UINT64_C(0xDDDDEEEEFFFF0000));
  REQUIRE_SESSION_CONTRACT(
      96, "render.guard.metadata-after-intact", session,
      metadata.after == UINT64_C(0x0000FFFFEEEEDDDD));
  REQUIRE_SESSION_CONTRACT(97, "render.guard.block-before-intact", session,
                           block.before == UINT64_C(0x13579BDF2468ACE0));
  REQUIRE_SESSION_CONTRACT(98, "render.guard.block-after-intact", session,
                           block.after == UINT64_C(0x0ECA8642FDB97531));
  REQUIRE_SESSION_CONTRACT(99, "render.guard.config-before-intact", session,
                           config.before == UINT64_C(0x1020304050607080));
  REQUIRE_SESSION_CONTRACT(100, "render.guard.config-after-intact", session,
                           config.after == UINT64_C(0x8070605040302010));
  REQUIRE_SESSION_CONTRACT(
      101, "render.guard.output-before-intact", session,
      output.before == UINT64_C(0x0102030405060708));
  REQUIRE_SESSION_CONTRACT(
      102, "render.guard.output-after-intact", session,
      output.after == UINT64_C(0x0807060504030201));

  float direct_after_valid[FB_MAX_PRESENTATION_FEEDS_V2 * 4];
  float environmental_after_valid[FB_MAX_ENVIRONMENTAL_CHANNELS_V2 * 4];
  struct FbPresentationFeedMetadataV2
      feeds_after_valid[FB_MAX_PRESENTATION_FEEDS_V2];
  const struct FbSpatialBlockMetadataV2 metadata_after_valid = metadata.value;
  memcpy(direct_after_valid, direct.samples, sizeof(direct_after_valid));
  memcpy(environmental_after_valid, environmental.samples,
         sizeof(environmental_after_valid));
  memcpy(feeds_after_valid, feeds.values, sizeof(feeds_after_valid));
  struct FbSpatialRenderBlockV2 missing_active_block = block.value;
  missing_active_block.source_program_count = 1;
  result = fb_session_render_spatial_v2(session, &missing_active_block);
  REQUIRE_SESSION_CONTRACT(
      80, "render.missing-active-source.result-invalid-argument", session,
      result == FbInvalidArgument);
  REQUIRE_SESSION_CONTRACT(
      103, "render.missing-active-source.direct-unchanged", session,
      memcmp(direct.samples, direct_after_valid, sizeof(direct_after_valid)) ==
          0);
  REQUIRE_SESSION_CONTRACT(
      104, "render.missing-active-source.environmental-unchanged", session,
      memcmp(environmental.samples, environmental_after_valid,
             sizeof(environmental_after_valid)) == 0);
  REQUIRE_SESSION_CONTRACT(
      105, "render.missing-active-source.feeds-unchanged", session,
      memcmp(feeds.values, feeds_after_valid, sizeof(feeds_after_valid)) == 0);
  REQUIRE_SESSION_CONTRACT(
      106, "render.missing-active-source.metadata-unchanged", session,
      memcmp(&metadata.value, &metadata_after_valid,
             sizeof(metadata_after_valid)) == 0);

  block.value.direct_output.reserved[0] = 1;
  result = fb_session_render_spatial_v2(session, &block.value);
  REQUIRE_SESSION_CONTRACT(
      38, "render.reserved-output-field.result-invalid-argument", session,
      result == FbInvalidArgument);
  REQUIRE_SESSION_CONTRACT(
      107, "render.reserved-output-field.direct-unchanged", session,
      memcmp(direct.samples, direct_after_valid, sizeof(direct_after_valid)) ==
          0);
  REQUIRE_SESSION_CONTRACT(
      108, "render.reserved-output-field.environmental-unchanged", session,
      memcmp(environmental.samples, environmental_after_valid,
             sizeof(environmental_after_valid)) == 0);
  REQUIRE_SESSION_CONTRACT(
      109, "render.reserved-output-field.feeds-unchanged", session,
      memcmp(feeds.values, feeds_after_valid, sizeof(feeds_after_valid)) == 0);
  REQUIRE_SESSION_CONTRACT(
      110, "render.reserved-output-field.metadata-unchanged", session,
      memcmp(&metadata.value, &metadata_after_valid,
             sizeof(metadata_after_valid)) == 0);

  block.value.direct_output.reserved[0] = 0;
  result = fb_session_render_spatial_v2(session, &block.value);
  REQUIRE_SESSION_CONTRACT(81, "render.second-valid-block.result-ok", session,
                           result == FbOk);
  REQUIRE_SESSION_CONTRACT(
      111, "render.metadata.block-start-frame.advanced", session,
      metadata.value.block_start_frame == 4);
  REQUIRE_SESSION_CONTRACT(
      39, "lifecycle.configure-after-render.invalid-state", session,
      fb_session_configure_source_v2(session, &source_one) == FbInvalidState);
  REQUIRE_CONTRACT(40, "lifecycle.destroy-neutral.result-ok",
                   fb_session_destroy(session) == FbOk);
  return 0;
}

static int run_v2_legacy_route_contract(const char *package_path,
                                        const char *bake_path) {
  const struct FbSessionConfigV2 config = {
      .abi_version = FB_ABI_VERSION_V2,
      .struct_size = sizeof(struct FbSessionConfigV2),
      .sample_rate_hz = 48000,
      .block_size_frames = 128,
      .source_count = 1,
      .default_source_level_db = 0.0f,
      .quality_tier = FbQualityDesktop,
      .render_route = FbRenderLegacyFinalStereoV2,
      .environmental_order = 0,
      .reserved = {0},
  };
  struct FbSession *session = NULL;
  const enum FbResult create_result =
      fb_session_create_v2(&config, package_path, bake_path, &session);
  REQUIRE_CONTRACT(50, "lifecycle.create-legacy.result-ok",
                   create_result == FbOk);
  REQUIRE_CONTRACT(112, "lifecycle.create-legacy.session-nonnull",
                   session != NULL);
  const struct FbSourceUpdate source = source_update_at(4.0f, 5.0f, 0.0f);
  const struct FbControlFrameV2 frame =
      control_frame(&source, 1, pose_at(0.0f, 0.0f, 0.0f));
  REQUIRE_SESSION_CONTRACT(
      66, "lifecycle.v2-created-legacy-control-frame.result-ok", session,
      fb_session_update_control_frame_v2(session, &frame) == FbOk);
  REQUIRE_SESSION_CONTRACT(
      51, "lifecycle.legacy-configure.invalid-state", session,
      fb_session_configure_source_v2(session, NULL) == FbInvalidState);
  REQUIRE_SESSION_CONTRACT(
      113, "lifecycle.legacy-prepare.invalid-state", session,
      fb_session_prepare_spatial_v2(session) == FbInvalidState);
  REQUIRE_SESSION_CONTRACT(
      114, "lifecycle.legacy-render.invalid-state", session,
      fb_session_render_spatial_v2(session, NULL) == FbInvalidState);
  REQUIRE_CONTRACT(52, "lifecycle.destroy-legacy.result-ok",
                   fb_session_destroy(session) == FbOk);
  return 0;
}

static int run_v1_legacy_batch_acceptance(const char *package_path,
                                          const char *bake_path) {
  const struct FbSessionConfig config = {
      .sample_rate_hz = 48000,
      .block_size_frames = 128,
      .source_count = 1,
      .default_source_level_db = 0.0f,
      .quality_tier = FbQualityDesktop,
  };
  struct FbSession *session = NULL;
  const enum FbResult create_result =
      fb_session_create(&config, package_path, bake_path, &session);
  enum FbResult batch_result = FbInvalidState;
  enum FbResult render_result = FbInvalidState;
  enum FbResult destroy_result = FbInvalidState;
  int output_finite = 0;
  if (session != NULL) {
    const struct FbSourceUpdate source = source_update_at(6.0f, 7.0f, 0.0f);
    const struct FbControlFrameV2 frame =
        control_frame(&source, 1, pose_at(0.0f, 0.0f, 0.0f));
    batch_result = fb_session_update_control_frame_v2(session, &frame);
    if (batch_result == FbOk) {
      float input[128];
      float output[256];
      for (size_t index = 0; index < 128; ++index) {
        input[index] = 0.25f;
      }
      for (size_t index = 0; index < 256; ++index) {
        output[index] = NAN;
      }
      render_result =
          fb_session_render_block(session, input, 128, output, 256);
      output_finite = samples_finite(output, 256);
    }
    destroy_result = fb_session_destroy(session);
  }
  REQUIRE_CONTRACT(
      115, "lifecycle.v1-created-legacy-control-frame.accepted",
      create_result == FbOk && session != NULL && batch_result == FbOk &&
          render_result == FbOk && output_finite && destroy_result == FbOk);
  return 0;
}

int main(int argc, char **argv) {
  REQUIRE_CONTRACT(10, "declaration.create-v2.nonnull", kCreateV2 != NULL);
  REQUIRE_CONTRACT(1, "declaration.configure-source-v2.nonnull",
                   kConfigureSourceV2 != NULL);
  REQUIRE_CONTRACT(116, "declaration.update-control-frame-v2.nonnull",
                   kUpdateControlFrameV2 != NULL);
  REQUIRE_CONTRACT(2, "declaration.prepare-spatial-v2.nonnull",
                   kPrepareSpatialV2 != NULL);
  REQUIRE_CONTRACT(3, "declaration.render-spatial-v2.nonnull",
                   kRenderSpatialV2 != NULL);
  const int rejection_result = validate_create_rejections();
  if (rejection_result != 0) {
    return rejection_result;
  }
  REQUIRE_CONTRACT(11, "invocation.asset-path-argument-count", argc == 3);
  const int neutral_result = run_neutral_binding(argv[1], argv[2]);
  if (neutral_result != 0) {
    return neutral_result;
  }
  const int v2_legacy_result =
      run_v2_legacy_route_contract(argv[1], argv[2]);
  if (v2_legacy_result != 0) {
    return v2_legacy_result;
  }
  return run_v1_legacy_batch_acceptance(argv[1], argv[2]);
}

#endif /* FB_V2_ADVERSARIAL_DIAGNOSTIC_SELF_TEST */
