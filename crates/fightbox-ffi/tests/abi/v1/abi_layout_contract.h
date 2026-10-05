#ifndef FIGHTBOX_V1_ABI_LAYOUT_CONTRACT_H
#define FIGHTBOX_V1_ABI_LAYOUT_CONTRACT_H

#include <stddef.h>

/* Rust exposes both enums with #[repr(u32)]. */
_Static_assert(sizeof(enum FbResult) == 4, "FbResult must remain a 32-bit enum");
_Static_assert(FbOk == 0, "FbOk value changed");
_Static_assert(FbInvalidArgument == 1, "FbInvalidArgument value changed");
_Static_assert(FbInvalidState == 2, "FbInvalidState value changed");
_Static_assert(FbIoError == 3, "FbIoError value changed");
_Static_assert(FbInvalidPackage == 4, "FbInvalidPackage value changed");
_Static_assert(FbInvalidBake == 5, "FbInvalidBake value changed");
_Static_assert(FbBackendUnavailable == 6, "FbBackendUnavailable value changed");
_Static_assert(FbBackendError == 7, "FbBackendError value changed");
_Static_assert(FbBufferTooSmall == 8, "FbBufferTooSmall value changed");
_Static_assert(FbPanic == 9, "FbPanic value changed");

_Static_assert(sizeof(enum FbQualityTier) == 4,
               "FbQualityTier must remain a 32-bit enum");
_Static_assert(FbQualityDesktop == 0, "FbQualityDesktop value changed");
_Static_assert(FbQualityMobile == 1, "FbQualityMobile value changed");

_Static_assert(sizeof(struct FbVec3) == 12, "FbVec3 size changed");
_Static_assert(_Alignof(struct FbVec3) == 4, "FbVec3 alignment changed");
_Static_assert(offsetof(struct FbVec3, east_m) == 0, "FbVec3.east_m moved");
_Static_assert(offsetof(struct FbVec3, north_m) == 4, "FbVec3.north_m moved");
_Static_assert(offsetof(struct FbVec3, up_m) == 8, "FbVec3.up_m moved");

_Static_assert(sizeof(struct FbPose) == 36, "FbPose size changed");
_Static_assert(_Alignof(struct FbPose) == 4, "FbPose alignment changed");
_Static_assert(offsetof(struct FbPose, position) == 0, "FbPose.position moved");
_Static_assert(offsetof(struct FbPose, forward) == 12, "FbPose.forward moved");
_Static_assert(offsetof(struct FbPose, up) == 24, "FbPose.up moved");

_Static_assert(sizeof(struct FbSessionConfig) == 20,
               "FbSessionConfig size changed; add a versioned config instead");
_Static_assert(_Alignof(struct FbSessionConfig) == 4,
               "FbSessionConfig alignment changed");
_Static_assert(offsetof(struct FbSessionConfig, sample_rate_hz) == 0,
               "FbSessionConfig.sample_rate_hz moved");
_Static_assert(offsetof(struct FbSessionConfig, block_size_frames) == 4,
               "FbSessionConfig.block_size_frames moved");
_Static_assert(offsetof(struct FbSessionConfig, source_count) == 8,
               "FbSessionConfig.source_count moved");
_Static_assert(offsetof(struct FbSessionConfig, default_source_level_db) == 12,
               "FbSessionConfig.default_source_level_db moved");
_Static_assert(offsetof(struct FbSessionConfig, quality_tier) == 16,
               "FbSessionConfig.quality_tier moved");

_Static_assert(sizeof(struct FbSourceUpdate) == 52, "FbSourceUpdate size changed");
_Static_assert(_Alignof(struct FbSourceUpdate) == 4,
               "FbSourceUpdate alignment changed");
_Static_assert(offsetof(struct FbSourceUpdate, active) == 0,
               "FbSourceUpdate.active moved");
_Static_assert(offsetof(struct FbSourceUpdate, pose) == 4,
               "FbSourceUpdate.pose moved");
_Static_assert(offsetof(struct FbSourceUpdate, linear_velocity_mps) == 40,
               "FbSourceUpdate.linear_velocity_mps moved");

#endif /* FIGHTBOX_V1_ABI_LAYOUT_CONTRACT_H */
