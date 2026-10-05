#ifndef FIGHTBOX_TAP_BRIDGE_H
#define FIGHTBOX_TAP_BRIDGE_H

#include <CoreAudio/CoreAudio.h>
#include <stdbool.h>
#include <stdint.h>
#include <sys/types.h>

typedef struct FatRing FatRing;

FatRing *fat_ring_create(const AudioStreamBasicDescription *format);
void fat_ring_destroy(FatRing *ring);
uint32_t fat_ring_pop(FatRing *ring, float *stereo, uint32_t max_frames);
uint64_t fat_ring_dropped(const FatRing *ring);
bool fat_ring_invalid(const FatRing *ring);
OSStatus fat_create_io(AudioObjectID device, FatRing *ring, AudioDeviceIOProcID *io);
OSStatus fat_io_proc(AudioObjectID device, const AudioTimeStamp *now,
                     const AudioBufferList *input, const AudioTimeStamp *input_time,
                     AudioBufferList *output, const AudioTimeStamp *output_time,
                     void *context);

bool fat_format_valid(const AudioStreamBasicDescription *format);
int fat_ring_self_test(void);
pid_t fat_parent_pid(pid_t pid);
int fat_process_path(pid_t pid, char *path, uint32_t capacity);
void fat_control_init(pid_t parent);
bool fat_control_stopped(void);
void fat_control_wait(int milliseconds);
bool fat_write_bytes(const uint8_t *bytes, uint32_t count);
void fat_write_final(const uint8_t *bytes, uint32_t count);

#endif
