#ifndef FIGHTBOX_SWIFTPM_SHIM_H
#define FIGHTBOX_SWIFTPM_SHIM_H

#include <stddef.h>
#include <stdint.h>

/* This package is intentionally rooted inside the Fightbox monorepo. */
#include "../../../../../../crates/fightbox-ffi/include/fightbox.h"

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Small C11-atomic mailbox shared by the asynchronous canonical-audio loader
 * and the realtime program provider. The decoded sample storage remains owned
 * by Swift; this object contains no PCM and performs no allocation after
 * creation.
 */
typedef struct FbCanonicalPlaybackAtomics FbCanonicalPlaybackAtomics;

FbCanonicalPlaybackAtomics *fb_canonical_playback_atomics_create(void);
void fb_canonical_playback_atomics_destroy(FbCanonicalPlaybackAtomics *atomics);

uint64_t fb_canonical_playhead_load(const FbCanonicalPlaybackAtomics *atomics);
void fb_canonical_playhead_store(FbCanonicalPlaybackAtomics *atomics, uint64_t frame);
uint64_t fb_canonical_playhead_advance(FbCanonicalPlaybackAtomics *atomics, uint64_t frames);
uint32_t fb_canonical_playhead_commit(
    FbCanonicalPlaybackAtomics *atomics,
    uint64_t expected_frame,
    uint64_t desired_frame,
    uint64_t expected_discontinuity_sequence
);
uint32_t fb_canonical_active_load(const FbCanonicalPlaybackAtomics *atomics);
void fb_canonical_active_store(FbCanonicalPlaybackAtomics *atomics, uint32_t active);
uint64_t fb_canonical_underrun_count(const FbCanonicalPlaybackAtomics *atomics);
void fb_canonical_record_underrun(FbCanonicalPlaybackAtomics *atomics);
uint64_t fb_canonical_discontinuity_load(const FbCanonicalPlaybackAtomics *atomics);
void fb_canonical_publish_seek(FbCanonicalPlaybackAtomics *atomics, uint64_t frame);

void fb_canonical_slot_begin_write(FbCanonicalPlaybackAtomics *atomics, uint32_t slot);
void fb_canonical_slot_publish(
    FbCanonicalPlaybackAtomics *atomics,
    uint32_t slot,
    uint64_t chunk_index
);
void fb_canonical_slot_invalidate(FbCanonicalPlaybackAtomics *atomics, uint32_t slot);
uint64_t fb_canonical_slot_sequence(
    const FbCanonicalPlaybackAtomics *atomics,
    uint32_t slot
);
uint64_t fb_canonical_slot_chunk_index(
    const FbCanonicalPlaybackAtomics *atomics,
    uint32_t slot
);

/**
 * Decode one canonical zstd chunk into an exactly sized caller buffer.
 * `libfightbox_ffi.a` already contains the repository-pinned zstd 1.5.7
 * implementation through zstd-sys; this shim deliberately adds no second
 * compressor implementation to the app.
 */
int32_t fb_canonical_zstd_decompress_exact(
    void *destination,
    size_t destination_size,
    const void *source,
    size_t source_size
);

#ifdef __cplusplus
}
#endif

#endif
