/*
 * Compile an implementation against the current generated header, then link
 * it to legacy_client.c, which was compiled against the frozen v1 header.
 * The definitions intentionally repeat the v1 signatures so incompatible
 * edits to the current declarations are compile errors.
 */
#include "fightbox.h"
#include "abi_layout_contract.h"
#include "abi_declaration_contract.h"

enum FbResult fb_session_create(const struct FbSessionConfig *config,
                                const char *package_path_utf8,
                                const char *bake_path_utf8,
                                struct FbSession **out_session) {
  (void)config;
  (void)package_path_utf8;
  (void)bake_path_utf8;
  if (out_session != NULL) {
    *out_session = NULL;
  }
  return FbInvalidArgument;
}

enum FbResult fb_session_update_listener(
    struct FbSession *session, const struct FbPose *pose,
    const struct FbVec3 *linear_velocity_mps) {
  (void)session;
  (void)pose;
  (void)linear_velocity_mps;
  return FbInvalidArgument;
}

enum FbResult fb_session_update_source(struct FbSession *session,
                                       uint32_t source_index,
                                       const struct FbSourceUpdate *update) {
  (void)session;
  (void)source_index;
  (void)update;
  return FbInvalidArgument;
}

enum FbResult fb_session_render_block(struct FbSession *session,
                                      const float *source_mono,
                                      size_t source_sample_count,
                                      float *out_interleaved_stereo,
                                      size_t out_sample_count) {
  (void)session;
  (void)source_mono;
  (void)source_sample_count;
  (void)out_interleaved_stereo;
  (void)out_sample_count;
  return FbInvalidArgument;
}

enum FbResult fb_session_telemetry_json(struct FbSession *session,
                                        char *buffer,
                                        size_t buffer_capacity,
                                        size_t *out_required) {
  (void)session;
  (void)buffer;
  (void)buffer_capacity;
  (void)out_required;
  return FbInvalidArgument;
}

enum FbResult fb_session_destroy(struct FbSession *session) {
  (void)session;
  return FbInvalidArgument;
}
