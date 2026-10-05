#include "fightbox_shim.h"

#include <stdatomic.h>
#include <stdlib.h>

#define FB_CANONICAL_CACHE_SLOT_COUNT 4u
#define FB_CANONICAL_INVALID_CHUNK UINT64_MAX

/* Supplied by the pinned zstd-sys objects inside libfightbox_ffi.a. */
extern size_t ZSTD_decompress(
    void *destination,
    size_t destination_capacity,
    const void *source,
    size_t source_size
);
extern unsigned ZSTD_isError(size_t code);

struct FbCanonicalPlaybackAtomics {
    _Atomic(uint64_t) playhead_frame;
    _Atomic(uint64_t) underrun_count;
    _Atomic(uint64_t) discontinuity_sequence;
    _Atomic(uint32_t) active;
    _Atomic(uint64_t) slot_sequence[FB_CANONICAL_CACHE_SLOT_COUNT];
    _Atomic(uint64_t) slot_chunk_index[FB_CANONICAL_CACHE_SLOT_COUNT];
};

static int fb_canonical_valid_slot(uint32_t slot) {
    return slot < FB_CANONICAL_CACHE_SLOT_COUNT;
}

FbCanonicalPlaybackAtomics *fb_canonical_playback_atomics_create(void) {
    FbCanonicalPlaybackAtomics *atomics = calloc(1, sizeof(*atomics));
    if (atomics == NULL) {
        return NULL;
    }
    atomic_init(&atomics->playhead_frame, 0);
    atomic_init(&atomics->underrun_count, 0);
    atomic_init(&atomics->discontinuity_sequence, 0);
    atomic_init(&atomics->active, 0);
    for (uint32_t slot = 0; slot < FB_CANONICAL_CACHE_SLOT_COUNT; ++slot) {
        atomic_init(&atomics->slot_sequence[slot], 0);
        atomic_init(&atomics->slot_chunk_index[slot], FB_CANONICAL_INVALID_CHUNK);
    }
    return atomics;
}

void fb_canonical_playback_atomics_destroy(FbCanonicalPlaybackAtomics *atomics) {
    free(atomics);
}

uint64_t fb_canonical_playhead_load(const FbCanonicalPlaybackAtomics *atomics) {
    return atomic_load_explicit(&atomics->playhead_frame, memory_order_acquire);
}

void fb_canonical_playhead_store(FbCanonicalPlaybackAtomics *atomics, uint64_t frame) {
    atomic_store_explicit(&atomics->playhead_frame, frame, memory_order_release);
}

uint64_t fb_canonical_playhead_advance(FbCanonicalPlaybackAtomics *atomics, uint64_t frames) {
    return atomic_fetch_add_explicit(&atomics->playhead_frame, frames, memory_order_acq_rel);
}

uint32_t fb_canonical_playhead_commit(
    FbCanonicalPlaybackAtomics *atomics,
    uint64_t expected_frame,
    uint64_t desired_frame,
    uint64_t expected_discontinuity_sequence
) {
    if ((expected_discontinuity_sequence & 1u) != 0u) {
        return 0;
    }
    uint64_t sequence = expected_discontinuity_sequence;
    if (!atomic_compare_exchange_strong_explicit(
        &atomics->discontinuity_sequence,
        &sequence,
        expected_discontinuity_sequence + 1,
        memory_order_acq_rel,
        memory_order_acquire
    )) {
        return 0;
    }
    uint32_t committed = atomic_compare_exchange_strong_explicit(
        &atomics->playhead_frame,
        &expected_frame,
        desired_frame,
        memory_order_acq_rel,
        memory_order_acquire
    );
    /* Ordinary advancement does not publish an externally visible epoch. */
    atomic_store_explicit(
        &atomics->discontinuity_sequence,
        expected_discontinuity_sequence,
        memory_order_release
    );
    return committed;
}

uint32_t fb_canonical_active_load(const FbCanonicalPlaybackAtomics *atomics) {
    return atomic_load_explicit(&atomics->active, memory_order_acquire);
}

void fb_canonical_active_store(FbCanonicalPlaybackAtomics *atomics, uint32_t active) {
    atomic_store_explicit(&atomics->active, active != 0, memory_order_release);
}

uint64_t fb_canonical_underrun_count(const FbCanonicalPlaybackAtomics *atomics) {
    return atomic_load_explicit(&atomics->underrun_count, memory_order_relaxed);
}

void fb_canonical_record_underrun(FbCanonicalPlaybackAtomics *atomics) {
    atomic_fetch_add_explicit(&atomics->underrun_count, 1, memory_order_relaxed);
}

uint64_t fb_canonical_discontinuity_load(const FbCanonicalPlaybackAtomics *atomics) {
    return atomic_load_explicit(&atomics->discontinuity_sequence, memory_order_acquire);
}

void fb_canonical_publish_seek(FbCanonicalPlaybackAtomics *atomics, uint64_t frame) {
    /* Odd means either a callback commit or seek publication is in progress. */
    uint64_t sequence;
    for (;;) {
        sequence = atomic_load_explicit(
            &atomics->discontinuity_sequence,
            memory_order_acquire
        );
        if ((sequence & 1u) != 0u) {
            continue;
        }
        uint64_t expected = sequence;
        if (atomic_compare_exchange_weak_explicit(
            &atomics->discontinuity_sequence,
            &expected,
            sequence + 1,
            memory_order_acq_rel,
            memory_order_acquire
        )) {
            break;
        }
    }
    atomic_store_explicit(&atomics->playhead_frame, frame, memory_order_release);
    atomic_store_explicit(
        &atomics->discontinuity_sequence,
        sequence + 2,
        memory_order_release
    );
}

void fb_canonical_slot_begin_write(FbCanonicalPlaybackAtomics *atomics, uint32_t slot) {
    if (!fb_canonical_valid_slot(slot)) {
        return;
    }
    atomic_fetch_add_explicit(&atomics->slot_sequence[slot], 1, memory_order_acq_rel);
    atomic_store_explicit(
        &atomics->slot_chunk_index[slot],
        FB_CANONICAL_INVALID_CHUNK,
        memory_order_release
    );
}

void fb_canonical_slot_publish(
    FbCanonicalPlaybackAtomics *atomics,
    uint32_t slot,
    uint64_t chunk_index
) {
    if (!fb_canonical_valid_slot(slot)) {
        return;
    }
    atomic_store_explicit(
        &atomics->slot_chunk_index[slot],
        chunk_index,
        memory_order_release
    );
    atomic_fetch_add_explicit(&atomics->slot_sequence[slot], 1, memory_order_release);
}

void fb_canonical_slot_invalidate(FbCanonicalPlaybackAtomics *atomics, uint32_t slot) {
    if (!fb_canonical_valid_slot(slot)) {
        return;
    }
    atomic_store_explicit(
        &atomics->slot_chunk_index[slot],
        FB_CANONICAL_INVALID_CHUNK,
        memory_order_release
    );
    uint64_t sequence = atomic_load_explicit(
        &atomics->slot_sequence[slot],
        memory_order_relaxed
    );
    if ((sequence & 1u) != 0u) {
        atomic_fetch_add_explicit(&atomics->slot_sequence[slot], 1, memory_order_release);
    }
}

uint64_t fb_canonical_slot_sequence(
    const FbCanonicalPlaybackAtomics *atomics,
    uint32_t slot
) {
    if (!fb_canonical_valid_slot(slot)) {
        return UINT64_MAX;
    }
    return atomic_load_explicit(&atomics->slot_sequence[slot], memory_order_acquire);
}

uint64_t fb_canonical_slot_chunk_index(
    const FbCanonicalPlaybackAtomics *atomics,
    uint32_t slot
) {
    if (!fb_canonical_valid_slot(slot)) {
        return FB_CANONICAL_INVALID_CHUNK;
    }
    return atomic_load_explicit(&atomics->slot_chunk_index[slot], memory_order_acquire);
}

int32_t fb_canonical_zstd_decompress_exact(
    void *destination,
    size_t destination_size,
    const void *source,
    size_t source_size
) {
    if (destination == NULL || source == NULL || destination_size == 0 || source_size == 0) {
        return -1;
    }
    size_t result = ZSTD_decompress(
        destination,
        destination_size,
        source,
        source_size
    );
    if (ZSTD_isError(result) || result != destination_size) {
        return -2;
    }
    return 0;
}
