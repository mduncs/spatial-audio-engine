#ifndef FIGHTBOX_V2_ABI_DECLARATION_CONTRACT_H
#define FIGHTBOX_V2_ABI_DECLARATION_CONTRACT_H

typedef enum FbResult (*FbCreateV2Signature)(
    const struct FbSessionConfigV2 *, const char *, const char *,
    struct FbSession **);
typedef enum FbResult (*FbConfigureSourceV2Signature)(
    struct FbSession *, const struct FbSourceProgramConfigV2 *);
typedef enum FbResult (*FbUpdateControlFrameV2Signature)(
    struct FbSession *, const struct FbControlFrameV2 *);
typedef enum FbResult (*FbPrepareSpatialV2Signature)(struct FbSession *);
typedef enum FbResult (*FbRenderSpatialV2Signature)(
    struct FbSession *, const struct FbSpatialRenderBlockV2 *);

#define FIGHTBOX_V2_ASSERT_SIGNATURE(symbol, signature)                       \
  _Static_assert(_Generic(&(symbol), signature : 1, default : 0),             \
                 #symbol " signature changed or declaration disappeared")

FIGHTBOX_V2_ASSERT_SIGNATURE(fb_session_create_v2, FbCreateV2Signature);
FIGHTBOX_V2_ASSERT_SIGNATURE(fb_session_configure_source_v2,
                             FbConfigureSourceV2Signature);
FIGHTBOX_V2_ASSERT_SIGNATURE(fb_session_update_control_frame_v2,
                             FbUpdateControlFrameV2Signature);
FIGHTBOX_V2_ASSERT_SIGNATURE(fb_session_prepare_spatial_v2,
                             FbPrepareSpatialV2Signature);
FIGHTBOX_V2_ASSERT_SIGNATURE(fb_session_render_spatial_v2,
                             FbRenderSpatialV2Signature);

#undef FIGHTBOX_V2_ASSERT_SIGNATURE

#endif /* FIGHTBOX_V2_ABI_DECLARATION_CONTRACT_H */
