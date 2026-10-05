#ifndef FIGHTBOX_V2_ABI_LAYOUT_CONTRACT_H
#define FIGHTBOX_V2_ABI_LAYOUT_CONTRACT_H

#include <stddef.h>
#include <stdint.h>

#define FB_V2_ASSERT_SIZE(type, expected)                                      \
  _Static_assert(sizeof(type) == (expected), #type " size changed")
#define FB_V2_ASSERT_ALIGN(type, expected)                                     \
  _Static_assert(_Alignof(type) == (expected), #type " alignment changed")
#define FB_V2_ASSERT_OFFSET(type, field, expected)                             \
  _Static_assert(offsetof(type, field) == (expected),                         \
                 #type "." #field " offset changed")

_Static_assert(sizeof(void *) == 8, "V2 Apple ABI requires 64-bit pointers");
_Static_assert(sizeof(size_t) == 8, "V2 Apple ABI requires 64-bit size_t");

_Static_assert(FB_ABI_VERSION_V2 == 2, "V2 ABI version changed");
_Static_assert(FB_MAX_PROGRAM_CHANNELS_V2 == 2,
               "program-channel capacity changed");
_Static_assert(FB_MAX_PRESENTATION_FEEDS_PER_SOURCE_V2 == 3,
               "per-source presentation capacity changed");
_Static_assert(FB_MAX_PRESENTATION_FEEDS_V2 == 48,
               "presentation-bank capacity changed");
_Static_assert(FB_MAX_ENVIRONMENTAL_ORDER_V2 == 2,
               "environmental-order capacity changed");
_Static_assert(FB_MAX_ENVIRONMENTAL_CHANNELS_V2 == 9,
               "environmental-bank capacity changed");
_Static_assert(FB_MAX_MULTIPOINT_COUNT_V2 == 255,
               "MultiPoint multiplicity changed");

_Static_assert(FB_PRESENTATION_PLANE_DIRECT_CENTER_OFFSET_V2 == 0,
               "center plane offset changed");
_Static_assert(FB_PRESENTATION_PLANE_WIDTH_POSITIVE_OFFSET_V2 == 1,
               "positive-width plane offset changed");
_Static_assert(FB_PRESENTATION_PLANE_WIDTH_NEGATIVE_OFFSET_V2 == 2,
               "negative-width plane offset changed");
_Static_assert(FB_PRESENTATION_COMPONENT_DIRECT_CENTER_V2 == (1u << 0),
               "center component bit changed");
_Static_assert(FB_PRESENTATION_COMPONENT_WIDTH_POSITIVE_V2 == (1u << 1),
               "positive-width component bit changed");
_Static_assert(FB_PRESENTATION_COMPONENT_WIDTH_NEGATIVE_V2 == (1u << 2),
               "negative-width component bit changed");
_Static_assert(FB_PRESENTATION_COMPONENT_DISCRETE_ECHO_V2 == (1u << 3),
               "echo reservation bit changed");

_Static_assert(FB_SPATIAL_BLOCK_VALID_V2 == (1u << 0),
               "validity flag changed");
_Static_assert(FB_SPATIAL_BLOCK_DISCONTINUITY_V2 == (1u << 1),
               "discontinuity flag changed");
_Static_assert(FB_SPATIAL_SOURCE_SAFETY_APPLIED_V2 == (1u << 2),
               "source-safety flag changed");
_Static_assert(FB_SPATIAL_OUTPUT_LIMITER_UNAPPLIED_V2 == (1u << 3),
               "limiter flag changed");
_Static_assert(FB_SPATIAL_WORLD_UNROTATED_V2 == (1u << 4),
               "world-space flag changed");
_Static_assert(FB_SPATIAL_FINAL_HRTF_UNAPPLIED_V2 == (1u << 5),
               "HRTF flag changed");
_Static_assert(FB_SPATIAL_SOURCE_DRIVE_APPLIED_V2 == (1u << 6),
               "source-drive flag changed");
_Static_assert(FB_SPATIAL_MONITOR_GAIN_UNAPPLIED_V2 == (1u << 7),
               "monitor-gain flag changed");

FB_V2_ASSERT_SIZE(FbRenderRouteV2, 4);
FB_V2_ASSERT_ALIGN(FbRenderRouteV2, 4);
_Static_assert(FbRenderLegacyFinalStereoV2 == 0, "legacy route changed");
_Static_assert(FbRenderNeutralSpatialV2 == 1, "neutral route changed");
FB_V2_ASSERT_SIZE(FbSourceGeometryV2, 4);
_Static_assert(FbSourceGeometryPointV2 == 0, "Point value changed");
_Static_assert(FbSourceGeometryMultiPointV2 == 1,
               "MultiPoint value changed");
_Static_assert(FbSourceGeometryLineSegmentV2 == 2,
               "LineSegment value changed");
_Static_assert(FbSourceGeometryStereoImageV2 == 3,
               "StereoImage value changed");
FB_V2_ASSERT_SIZE(FbEnvironmentalChannelOrderV2, 4);
_Static_assert(FbEnvironmentalAcnV2 == 0, "ACN value changed");
FB_V2_ASSERT_SIZE(FbEnvironmentalNormalizationV2, 4);
_Static_assert(FbEnvironmentalN3dV2 == 0, "N3D value changed");
FB_V2_ASSERT_SIZE(FbEnvironmentalBasisV2, 4);
_Static_assert(FbEnvironmentalRightHandedEnuV2 == 0,
               "ENU basis value changed");
_Static_assert(FbEnvironmentalSteamXRightYUpZBackV2 == 1,
               "Steam basis value changed");
FB_V2_ASSERT_SIZE(FbSpatialOutputValidityV2, 4);
_Static_assert(FbSpatialInvalidV2 == 0, "invalid output value changed");
_Static_assert(FbSpatialValidV2 == 1, "valid output value changed");
_Static_assert(FbSpatialSilentDiscontinuityV2 == 2,
               "silent-discontinuity value changed");
FB_V2_ASSERT_SIZE(FbPresentationPlacementV2, 4);
_Static_assert(FbPresentationPoseV2 == 0, "pose placement value changed");
_Static_assert(FbPresentationDirectionV2 == 1,
               "direction placement value changed");

FB_V2_ASSERT_SIZE(struct FbSessionConfigV2, 64);
FB_V2_ASSERT_ALIGN(struct FbSessionConfigV2, 4);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, abi_version, 0);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, struct_size, 4);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, sample_rate_hz, 8);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, block_size_frames, 12);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, source_count, 16);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, default_source_level_db, 20);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, quality_tier, 24);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, render_route, 28);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, environmental_order, 32);
FB_V2_ASSERT_OFFSET(struct FbSessionConfigV2, reserved, 36);

FB_V2_ASSERT_SIZE(struct FbSourceProgramConfigV2, 64);
FB_V2_ASSERT_ALIGN(struct FbSourceProgramConfigV2, 8);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2, abi_version, 0);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2, struct_size, 4);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2, source_index, 8);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2, channel_count, 12);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2, source_geometry, 16);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2,
                    multipoint_count, 20);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2, extent_m, 24);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2, reserved_f32, 28);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramConfigV2, reserved, 32);

FB_V2_ASSERT_SIZE(struct FbSourceProgramInputV2, 64);
FB_V2_ASSERT_ALIGN(struct FbSourceProgramInputV2, 8);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramInputV2, source_index, 0);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramInputV2, channel_count, 4);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramInputV2, samples, 8);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramInputV2, sample_count, 16);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramInputV2, channel_stride_samples, 24);
FB_V2_ASSERT_OFFSET(struct FbSourceProgramInputV2, reserved, 32);

FB_V2_ASSERT_SIZE(struct FbPlanarOutputV2, 64);
FB_V2_ASSERT_ALIGN(struct FbPlanarOutputV2, 8);
FB_V2_ASSERT_OFFSET(struct FbPlanarOutputV2, samples, 0);
FB_V2_ASSERT_OFFSET(struct FbPlanarOutputV2, sample_capacity, 8);
FB_V2_ASSERT_OFFSET(struct FbPlanarOutputV2, plane_capacity, 16);
FB_V2_ASSERT_OFFSET(struct FbPlanarOutputV2, reserved_u32, 20);
FB_V2_ASSERT_OFFSET(struct FbPlanarOutputV2, plane_stride_samples, 24);
FB_V2_ASSERT_OFFSET(struct FbPlanarOutputV2, reserved, 32);

FB_V2_ASSERT_SIZE(struct FbPresentationFeedMetadataV2, 80);
FB_V2_ASSERT_ALIGN(struct FbPresentationFeedMetadataV2, 4);
FB_V2_ASSERT_OFFSET(struct FbPresentationFeedMetadataV2, valid, 0);
FB_V2_ASSERT_OFFSET(struct FbPresentationFeedMetadataV2, source_index, 4);
FB_V2_ASSERT_OFFSET(struct FbPresentationFeedMetadataV2, component, 8);
FB_V2_ASSERT_OFFSET(struct FbPresentationFeedMetadataV2, placement, 12);
FB_V2_ASSERT_OFFSET(struct FbPresentationFeedMetadataV2, pose, 16);
FB_V2_ASSERT_OFFSET(struct FbPresentationFeedMetadataV2, direction_enu, 52);
FB_V2_ASSERT_OFFSET(struct FbPresentationFeedMetadataV2,
                    processing_latency_frames, 64);
FB_V2_ASSERT_OFFSET(struct FbPresentationFeedMetadataV2, reserved, 68);

FB_V2_ASSERT_SIZE(struct FbSpatialBlockMetadataV2, 120);
FB_V2_ASSERT_ALIGN(struct FbSpatialBlockMetadataV2, 8);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, abi_version, 0);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, struct_size, 4);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, sample_rate_hz, 8);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, block_size_frames, 12);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, block_start_frame, 16);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, generation, 24);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, discontinuity_sequence,
                    32);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, validity, 40);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2,
                    active_presentation_feed_count, 44);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, environmental_order, 48);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2,
                    environmental_channel_count, 52);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2,
                    environmental_latency_frames, 56);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, component_mask, 60);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, flags, 64);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2,
                    environmental_channel_order, 68);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2,
                    environmental_normalization, 72);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, environmental_basis, 76);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, reserved_u32, 80);
FB_V2_ASSERT_OFFSET(struct FbSpatialBlockMetadataV2, reserved, 88);

FB_V2_ASSERT_SIZE(struct FbControlFrameV2, 104);
FB_V2_ASSERT_ALIGN(struct FbControlFrameV2, 8);
FB_V2_ASSERT_OFFSET(struct FbControlFrameV2, abi_version, 0);
FB_V2_ASSERT_OFFSET(struct FbControlFrameV2, struct_size, 4);
FB_V2_ASSERT_OFFSET(struct FbControlFrameV2, source_updates, 8);
FB_V2_ASSERT_OFFSET(struct FbControlFrameV2, source_count, 16);
FB_V2_ASSERT_OFFSET(struct FbControlFrameV2,
                    source_update_stride_bytes, 20);
FB_V2_ASSERT_OFFSET(struct FbControlFrameV2, listener_pose, 24);
FB_V2_ASSERT_OFFSET(struct FbControlFrameV2,
                    listener_linear_velocity_mps, 60);
FB_V2_ASSERT_OFFSET(struct FbControlFrameV2, reserved, 72);

FB_V2_ASSERT_SIZE(struct FbSpatialRenderBlockV2, 208);
FB_V2_ASSERT_ALIGN(struct FbSpatialRenderBlockV2, 8);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, abi_version, 0);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, struct_size, 4);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, source_programs, 8);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, source_program_count, 16);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2,
                    source_program_stride_bytes, 20);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, direct_output, 24);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, environmental_output, 88);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, feed_metadata, 152);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, feed_metadata_capacity, 160);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2,
                    feed_metadata_stride_bytes, 164);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, block_metadata, 168);
FB_V2_ASSERT_OFFSET(struct FbSpatialRenderBlockV2, reserved, 176);

#undef FB_V2_ASSERT_OFFSET
#undef FB_V2_ASSERT_ALIGN
#undef FB_V2_ASSERT_SIZE

#endif /* FIGHTBOX_V2_ABI_LAYOUT_CONTRACT_H */
