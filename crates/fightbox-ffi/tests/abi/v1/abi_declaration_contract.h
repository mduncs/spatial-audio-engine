#ifndef FIGHTBOX_V1_ABI_DECLARATION_CONTRACT_H
#define FIGHTBOX_V1_ABI_DECLARATION_CONTRACT_H

typedef enum FbResult (*FbCreateSignature)(const struct FbSessionConfig *,
                                           const char *, const char *,
                                           struct FbSession **);
typedef enum FbResult (*FbUpdateListenerSignature)(struct FbSession *,
                                                   const struct FbPose *,
                                                   const struct FbVec3 *);
typedef enum FbResult (*FbUpdateSourceSignature)(struct FbSession *, uint32_t,
                                                 const struct FbSourceUpdate *);
typedef enum FbResult (*FbRenderSignature)(struct FbSession *, const float *,
                                           size_t, float *, size_t);
typedef enum FbResult (*FbTelemetrySignature)(struct FbSession *, char *, size_t,
                                              size_t *);
typedef enum FbResult (*FbDestroySignature)(struct FbSession *);

/*
 * Referencing each function before any fixture definition also proves that
 * the included public header still declares the old symbol. _Generic makes an
 * incompatible declaration a hard C11 static-assert failure.
 */
#define FIGHTBOX_V1_ASSERT_SIGNATURE(symbol, signature)                        \
  _Static_assert(_Generic(&(symbol), signature: 1, default: 0),                \
                 #symbol " signature changed or declaration disappeared")

FIGHTBOX_V1_ASSERT_SIGNATURE(fb_session_create, FbCreateSignature);
FIGHTBOX_V1_ASSERT_SIGNATURE(fb_session_update_listener,
                             FbUpdateListenerSignature);
FIGHTBOX_V1_ASSERT_SIGNATURE(fb_session_update_source, FbUpdateSourceSignature);
FIGHTBOX_V1_ASSERT_SIGNATURE(fb_session_render_block, FbRenderSignature);
FIGHTBOX_V1_ASSERT_SIGNATURE(fb_session_telemetry_json, FbTelemetrySignature);
FIGHTBOX_V1_ASSERT_SIGNATURE(fb_session_destroy, FbDestroySignature);

#undef FIGHTBOX_V1_ASSERT_SIGNATURE

#endif /* FIGHTBOX_V1_ABI_DECLARATION_CONTRACT_H */
