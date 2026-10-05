#include <stddef.h>

#include "../../Sources/FightboxC/include/fightbox_shim.h"

size_t ZSTD_decompress(
    void *destination,
    size_t destination_capacity,
    const void *source,
    size_t source_size
) {
    (void)destination;
    (void)destination_capacity;
    (void)source;
    (void)source_size;
    return (size_t)-1;
}

unsigned ZSTD_isError(size_t code) {
    (void)code;
    return 1;
}

enum FbResult fb_session_create(
    const struct FbSessionConfig *config,
    const char *package_path_utf8,
    const char *bake_path_utf8,
    struct FbSession **out_session
) {
    (void)config;
    (void)package_path_utf8;
    (void)bake_path_utf8;
    if (out_session != NULL) *out_session = NULL;
    return FbInvalidState;
}

enum FbResult fb_session_create_v2(
    const struct FbSessionConfigV2 *config,
    const char *package_path_utf8,
    const char *bake_path_utf8,
    struct FbSession **out_session
) {
    (void)config;
    (void)package_path_utf8;
    (void)bake_path_utf8;
    if (out_session != NULL) *out_session = NULL;
    return FbInvalidState;
}

enum FbResult fb_session_configure_source_v2(
    struct FbSession *session,
    const struct FbSourceProgramConfigV2 *config
) {
    (void)session;
    (void)config;
    return FbInvalidState;
}

enum FbResult fb_session_prepare_spatial_v2(struct FbSession *session) {
    (void)session;
    return FbInvalidState;
}

enum FbResult fb_session_prepare_cell_v2(
    struct FbSession *session,
    const char *package_path_utf8,
    const char *bake_path_utf8,
    const struct FbCellConfigV2 *config,
    struct FbPreparedCell **out_prepared_cell
) {
    (void)session;
    (void)package_path_utf8;
    (void)bake_path_utf8;
    (void)config;
    if (out_prepared_cell != NULL) *out_prepared_cell = NULL;
    return FbInvalidState;
}

enum FbResult fb_session_offer_prepared_cell_v2(
    struct FbSession *session,
    struct FbPreparedCell *prepared_cell
) {
    (void)session;
    (void)prepared_cell;
    return FbInvalidState;
}

enum FbResult fb_session_cell_stream_state_v2(
    struct FbSession *session,
    struct FbCellStreamStateV2 *out_state
) {
    (void)session;
    (void)out_state;
    return FbInvalidState;
}

enum FbResult fb_session_collect_retired_cell_v2(struct FbSession *session) {
    (void)session;
    return FbInvalidState;
}

enum FbResult fb_prepared_cell_destroy_v2(struct FbPreparedCell *prepared_cell) {
    (void)prepared_cell;
    return FbInvalidState;
}

enum FbResult fb_session_render_spatial_v2(
    struct FbSession *session,
    const struct FbSpatialRenderBlockV2 *block
) {
    (void)session;
    (void)block;
    return FbInvalidState;
}

enum FbResult fb_session_update_listener(
    struct FbSession *session,
    const struct FbPose *pose,
    const struct FbVec3 *linear_velocity_mps
) {
    (void)session;
    (void)pose;
    (void)linear_velocity_mps;
    return FbInvalidState;
}

enum FbResult fb_session_update_source(
    struct FbSession *session,
    uint32_t source_index,
    const struct FbSourceUpdate *update
) {
    (void)session;
    (void)source_index;
    (void)update;
    return FbInvalidState;
}

enum FbResult fb_session_update_control_frame_v2(
    struct FbSession *session,
    const struct FbControlFrameV2 *frame
) {
    (void)session;
    (void)frame;
    return FbInvalidState;
}

enum FbResult fb_session_render_block(
    struct FbSession *session,
    const float *source_mono,
    size_t source_sample_count,
    float *out_interleaved_stereo,
    size_t out_sample_count
) {
    (void)session;
    (void)source_mono;
    (void)source_sample_count;
    (void)out_interleaved_stereo;
    (void)out_sample_count;
    return FbInvalidState;
}

enum FbResult fb_session_telemetry_json(
    struct FbSession *session,
    char *buffer,
    size_t buffer_capacity,
    size_t *out_required
) {
    (void)session;
    (void)buffer;
    (void)buffer_capacity;
    if (out_required != NULL) *out_required = 0;
    return FbInvalidState;
}

enum FbResult fb_session_destroy(struct FbSession *session) {
    (void)session;
    return FbInvalidState;
}


enum FbResult fb_session_admit_macro_event_group_v2(
    struct FbSession *session,
    const struct FbMacroEventRequestV2 *requests,
    uint32_t request_count
) {
    (void)session; (void)requests; (void)request_count;
    return FbInvalidState;
}

enum FbResult fb_session_enable_macro_production_bridge_v3(
    struct FbSession *session,
    const struct FbMacroProductionBridgeConfigV3 *config
) {
    (void)session; (void)config;
    return FbInvalidState;
}

enum FbResult fb_session_bind_macro_echo_anchor_v3(
    struct FbSession *session,
    const struct FbMacroEchoAnchorBindingV3 *binding
) {
    (void)session; (void)binding;
    return FbInvalidState;
}

enum FbResult fb_session_prepare_macro_token_v3(
    struct FbSession *session,
    uint64_t lookahead_frame,
    struct FbMacroPrepareBatchV3 *out_batch
) {
    (void)session; (void)lookahead_frame; (void)out_batch;
    return FbInvalidState;
}

enum FbResult fb_session_stage_macro_ready_v3(
    struct FbSession *session,
    const struct FbMacroReadyAssetV3 *ready,
    uint32_t ready_count
) {
    (void)session; (void)ready; (void)ready_count;
    return FbInvalidState;
}

enum FbResult fb_session_update_control_frame_macro_v3(
    struct FbSession *session,
    const struct FbControlFrameV2 *frame,
    uint64_t token_id,
    struct FbMacroCommitResultV3 *out_result
) {
    (void)session; (void)frame; (void)token_id; (void)out_result;
    return FbInvalidState;
}

enum FbResult fb_session_discard_macro_token_v3(
    struct FbSession *session,
    uint64_t token_id
) {
    (void)session; (void)token_id;
    return FbInvalidState;
}

enum FbResult fb_session_poll_macro_ack_v3(
    struct FbSession *session,
    struct FbMacroAudioAckV3 *out_ack
) {
    (void)session; (void)out_ack;
    return FbInvalidState;
}

enum FbResult fb_session_finalize_macro_ack_v3(
    struct FbSession *session,
    const struct FbMacroAudioAckV3 *ack
) {
    (void)session; (void)ack;
    return FbInvalidState;
}

enum FbResult fb_session_macro_render_begin_v3(
    struct FbSession *session,
    struct FbMacroProgramRequestBatchV3 *out_batch
) {
    (void)session; (void)out_batch;
    return FbInvalidState;
}

enum FbResult fb_session_macro_render_end_v3(
    struct FbSession *session,
    uint32_t disposition
) {
    (void)session; (void)disposition;
    return FbInvalidState;
}
