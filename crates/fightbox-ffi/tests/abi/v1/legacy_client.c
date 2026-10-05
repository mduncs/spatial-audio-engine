#include "fightbox_v1.h"
#include "abi_layout_contract.h"
#include "abi_declaration_contract.h"

#include <stdint.h>

/* Incompatible source signatures fail this translation unit under -Werror. */
static const FbCreateSignature kCreate = &fb_session_create;
static const FbUpdateListenerSignature kUpdateListener = &fb_session_update_listener;
static const FbUpdateSourceSignature kUpdateSource = &fb_session_update_source;
static const FbRenderSignature kRender = &fb_session_render_block;
static const FbTelemetrySignature kTelemetry = &fb_session_telemetry_json;
static const FbDestroySignature kDestroy = &fb_session_destroy;

static int all_v1_symbols_are_linked(void) {
  return kCreate != NULL && kUpdateListener != NULL && kUpdateSource != NULL &&
         kRender != NULL && kTelemetry != NULL && kDestroy != NULL;
}

static struct FbVec3 vector(float east, float north, float up) {
  const struct FbVec3 value = {
      .east_m = east,
      .north_m = north,
      .up_m = up,
  };
  return value;
}

static struct FbPose pose(float east, float north, float up) {
  const struct FbPose value = {
      .position = vector(east, north, up),
      .forward = vector(0.0f, 1.0f, 0.0f),
      .up = vector(0.0f, 0.0f, 1.0f),
  };
  return value;
}

static int run_real_session_smoke(const char *package_path, const char *bake_path) {
  const struct FbSessionConfig config = {
      .sample_rate_hz = 48000,
      .block_size_frames = 128,
      .source_count = 1,
      .default_source_level_db = 0.0f,
      .quality_tier = FbQualityDesktop,
  };
  const struct FbPose listener = pose(0.0f, 0.0f, 1.7f);
  const struct FbVec3 listener_velocity = vector(0.0f, 0.0f, 0.0f);
  const struct FbSourceUpdate source = {
      .active = 1,
      .pose = pose(1.0f, 0.0f, 1.7f),
      .linear_velocity_mps = {0.0f, 0.0f, 0.0f},
  };
  float input[128] = {0.0f};
  float output[256] = {0.0f};
  struct FbSession *session = NULL;

  if (fb_session_create(&config, package_path, bake_path, &session) != FbOk ||
      session == NULL) {
    return 20;
  }
  if (fb_session_update_listener(session, &listener, &listener_velocity) != FbOk) {
    (void)fb_session_destroy(session);
    return 21;
  }
  if (fb_session_update_source(session, 0, &source) != FbOk) {
    (void)fb_session_destroy(session);
    return 22;
  }
  if (fb_session_render_block(session, input, 128, output, 256) != FbOk) {
    (void)fb_session_destroy(session);
    return 23;
  }
  if (fb_session_destroy(session) != FbOk) {
    return 24;
  }
  return 0;
}

int main(int argc, char **argv) {
  struct FbSession *session = (struct FbSession *)(uintptr_t)1;
  float sample = 0.0f;
  size_t required = 7;

  if (!all_v1_symbols_are_linked()) {
    return 10;
  }
  if (fb_session_create(NULL, NULL, NULL, &session) != FbInvalidArgument ||
      session != NULL) {
    return 11;
  }
  if (fb_session_update_listener(NULL, NULL, NULL) != FbInvalidArgument) {
    return 12;
  }
  if (fb_session_update_source(NULL, 0, NULL) != FbInvalidArgument) {
    return 13;
  }
  if (fb_session_render_block(NULL, &sample, 1, &sample, 1) !=
      FbInvalidArgument) {
    return 14;
  }
  if (fb_session_telemetry_json(NULL, NULL, 0, &required) !=
      FbInvalidArgument) {
    return 15;
  }
  if (fb_session_destroy(NULL) != FbInvalidArgument) {
    return 16;
  }
  if (argc == 3) {
    return run_real_session_smoke(argv[1], argv[2]);
  }
  if (argc != 1) {
    return 17;
  }
  return 0;
}
