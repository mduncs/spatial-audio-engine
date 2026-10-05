#include "TapBridge.h"

#include <errno.h>
#include <fcntl.h>
#include <libproc.h>
#include <math.h>
#include <poll.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define RING_FRAMES 32768u
#define MAX_CHANNELS 64u

struct FatRing {
    _Atomic uint32_t write_frame;
    _Atomic uint32_t read_frame;
    _Atomic uint64_t dropped;
    _Atomic bool invalid;
    uint32_t channels;
    float samples[RING_FRAMES * 2];
};

bool fat_format_valid(const AudioStreamBasicDescription *format) {
    if (!format || format->mFormatID != kAudioFormatLinearPCM ||
        !(format->mFormatFlags & kAudioFormatFlagIsFloat) ||
        !(format->mFormatFlags & kAudioFormatFlagIsPacked) ||
        (format->mFormatFlags & (kAudioFormatFlagIsBigEndian | kAudioFormatFlagIsAlignedHigh)) ||
        format->mBitsPerChannel != 32 || format->mFramesPerPacket != 1 ||
        format->mChannelsPerFrame == 0 || format->mChannelsPerFrame > MAX_CHANNELS ||
        !isfinite(format->mSampleRate) || format->mSampleRate < 8000 ||
        format->mSampleRate > 384000 ||
        fabs(format->mSampleRate - round(format->mSampleRate)) > 0.001) return false;
    uint32_t channels = (format->mFormatFlags & kAudioFormatFlagIsNonInterleaved)
        ? 1 : format->mChannelsPerFrame;
    return format->mBytesPerFrame == sizeof(float) * channels &&
        format->mBytesPerPacket == format->mBytesPerFrame;
}

FatRing *fat_ring_create(const AudioStreamBasicDescription *format) {
    if (!fat_format_valid(format)) return NULL;
    FatRing *ring = calloc(1, sizeof(FatRing));
    if (!ring) return NULL;
    atomic_init(&ring->write_frame, 0);
    atomic_init(&ring->read_frame, 0);
    atomic_init(&ring->dropped, 0);
    atomic_init(&ring->invalid, false);
    if (!atomic_is_lock_free(&ring->write_frame) || !atomic_is_lock_free(&ring->read_frame) ||
        !atomic_is_lock_free(&ring->dropped) || !atomic_is_lock_free(&ring->invalid)) {
        free(ring);
        return NULL;
    }
    ring->channels = format->mChannelsPerFrame;
    return ring;
}

void fat_ring_destroy(FatRing *ring) { free(ring); }

static void push_input(FatRing *ring, const AudioBufferList *input) {
    if (!input || !input->mNumberBuffers) return;
    if (input->mNumberBuffers > MAX_CHANNELS) {
        atomic_store_explicit(&ring->invalid, true, memory_order_relaxed);
        return;
    }
    uint32_t channels = 0, frames = UINT32_MAX;
    for (uint32_t index = 0; index < input->mNumberBuffers; ++index) {
        const AudioBuffer *buffer = &input->mBuffers[index];
        if (!buffer->mNumberChannels || buffer->mNumberChannels > MAX_CHANNELS ||
            buffer->mDataByteSize % (sizeof(float) * buffer->mNumberChannels)) {
            atomic_store_explicit(&ring->invalid, true, memory_order_relaxed);
            return;
        }
        channels += buffer->mNumberChannels;
        uint32_t buffer_frames = buffer->mDataByteSize / (sizeof(float) * buffer->mNumberChannels);
        if (buffer_frames < frames) frames = buffer_frames;
    }
    if (channels != ring->channels) {
        atomic_store_explicit(&ring->invalid, true, memory_order_relaxed);
        return;
    }
    uint32_t write_frame = atomic_load_explicit(&ring->write_frame, memory_order_relaxed);
    uint32_t read_frame = atomic_load_explicit(&ring->read_frame, memory_order_acquire);
    uint32_t available = RING_FRAMES - (write_frame - read_frame);
    uint32_t accepted = frames < available ? frames : available;
    uint32_t left_count = (channels + 1) / 2, right_count = channels / 2;
    for (uint32_t frame = 0; frame < accepted; ++frame) {
        float left = 0, right = 0;
        uint32_t channel_base = 0;
        for (uint32_t index = 0; index < input->mNumberBuffers; ++index) {
            const AudioBuffer *buffer = &input->mBuffers[index];
            const float *samples = buffer->mData;
            for (uint32_t channel = 0; channel < buffer->mNumberChannels; ++channel) {
                float sample = samples ? samples[frame * buffer->mNumberChannels + channel] : 0;
                if (!isfinite(sample)) sample = 0;
                if ((channel_base + channel) % 2 == 0) left += sample;
                else right += sample;
            }
            channel_base += buffer->mNumberChannels;
        }
        uint32_t slot = ((write_frame + frame) & (RING_FRAMES - 1)) * 2;
        ring->samples[slot] = left / left_count;
        ring->samples[slot + 1] = right_count ? right / right_count : ring->samples[slot];
    }
    atomic_store_explicit(&ring->write_frame, write_frame + accepted, memory_order_release);
    if (accepted < frames) atomic_fetch_add_explicit(&ring->dropped, frames - accepted, memory_order_relaxed);
}

OSStatus fat_io_proc(AudioObjectID device, const AudioTimeStamp *now,
                     const AudioBufferList *input, const AudioTimeStamp *input_time,
                     AudioBufferList *output, const AudioTimeStamp *output_time,
                     void *context) {
    (void)device; (void)now; (void)input_time; (void)output; (void)output_time;
    if (context) push_input(context, input);
    return noErr;
}

OSStatus fat_create_io(AudioObjectID device, FatRing *ring, AudioDeviceIOProcID *io) {
    return AudioDeviceCreateIOProcID(device, fat_io_proc, ring, io);
}

uint32_t fat_ring_pop(FatRing *ring, float *stereo, uint32_t max_frames) {
    uint32_t read_frame = atomic_load_explicit(&ring->read_frame, memory_order_relaxed);
    uint32_t write_frame = atomic_load_explicit(&ring->write_frame, memory_order_acquire);
    uint32_t frames = write_frame - read_frame;
    if (frames > max_frames) frames = max_frames;
    for (uint32_t frame = 0; frame < frames; ++frame) {
        uint32_t slot = ((read_frame + frame) & (RING_FRAMES - 1)) * 2;
        stereo[frame * 2] = ring->samples[slot];
        stereo[frame * 2 + 1] = ring->samples[slot + 1];
    }
    atomic_store_explicit(&ring->read_frame, read_frame + frames, memory_order_release);
    return frames;
}

uint64_t fat_ring_dropped(const FatRing *ring) {
    return atomic_load_explicit(&ring->dropped, memory_order_relaxed);
}

bool fat_ring_invalid(const FatRing *ring) {
    return atomic_load_explicit(&ring->invalid, memory_order_relaxed);
}

pid_t fat_parent_pid(pid_t pid) {
    struct proc_bsdinfo info = {0};
    return proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == sizeof(info)
        ? (pid_t)info.pbi_ppid : 0;
}

int fat_process_path(pid_t pid, char *path, uint32_t capacity) {
    return proc_pidpath(pid, path, capacity);
}

static volatile sig_atomic_t stop_requested = 0;
static pid_t parent_pid = 0;
static char command[5];
static unsigned command_length = 0;

static void stop_signal(int signal_number) { (void)signal_number; stop_requested = 1; }

void fat_control_init(pid_t parent) {
    parent_pid = parent;
    signal(SIGPIPE, SIG_IGN);
    signal(SIGTERM, stop_signal);
    signal(SIGINT, stop_signal);
    signal(SIGHUP, stop_signal);
    fcntl(STDIN_FILENO, F_SETFL, fcntl(STDIN_FILENO, F_GETFL) | O_NONBLOCK);
    fcntl(STDOUT_FILENO, F_SETFL, fcntl(STDOUT_FILENO, F_GETFL) | O_NONBLOCK);
}

bool fat_control_stopped(void) {
    if (stop_requested || (parent_pid > 1 && getppid() != parent_pid)) return true;
    char bytes[64];
    ssize_t count;
    while ((count = read(STDIN_FILENO, bytes, sizeof(bytes))) > 0) {
        for (ssize_t index = 0; index < count; ++index) {
            char byte = bytes[index];
            if (byte == '\n' || byte == '\r') command_length = 0;
            else if (command_length < 4) {
                command[command_length++] = byte;
                if (command_length == 4 && memcmp(command, "stop", 4) == 0) stop_requested = 1;
            }
        }
    }
    if (count == 0 || (count < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR))
        stop_requested = 1;
    return stop_requested;
}

void fat_control_wait(int milliseconds) {
    struct pollfd fd = {.fd = STDIN_FILENO, .events = POLLIN};
    poll(&fd, 1, milliseconds);
}

bool fat_write_bytes(const uint8_t *bytes, uint32_t count) {
    uint32_t offset = 0;
    while (offset < count) {
        if (fat_control_stopped()) return false;
        ssize_t sent = write(STDOUT_FILENO, bytes + offset, count - offset);
        if (sent > 0) offset += (uint32_t)sent;
        else if (sent < 0 && errno == EINTR) continue;
        else if (sent < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
            struct pollfd fds[2] = {
                {.fd = STDIN_FILENO, .events = POLLIN},
                {.fd = STDOUT_FILENO, .events = POLLOUT}
            };
            poll(fds, 2, 20);
        } else { stop_requested = 1; return false; }
    }
    return true;
}

void fat_write_final(const uint8_t *bytes, uint32_t count) {
    // Best effort after stop: stdout is nonblocking and the terminal status is 16 bytes.
    (void)write(STDOUT_FILENO, bytes, count);
}

int fat_ring_self_test(void) {
    AudioStreamBasicDescription format = {
        .mSampleRate = 44100, .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        .mBytesPerPacket = 8, .mFramesPerPacket = 1, .mBytesPerFrame = 8,
        .mChannelsPerFrame = 2, .mBitsPerChannel = 32
    };
    FatRing *ring = fat_ring_create(&format);
    if (!ring) return 1;
    float source[] = {0.25f, -0.5f, 0.75f, -0.125f, NAN, INFINITY}, out[8] = {0};
    AudioBufferList input = {.mNumberBuffers = 1, .mBuffers = {{2, sizeof(source), source}}};
    fat_io_proc(0, NULL, &input, NULL, NULL, NULL, ring);
    if (fat_ring_pop(ring, out, 4) != 3 || out[0] != source[0] || out[1] != source[1] ||
        out[2] != source[2] || out[3] != source[3] || out[4] != 0 || out[5] != 0) return 2;
    // Exercise wraparound and full-ring drop without advancing the consumer.
    atomic_store(&ring->write_frame, UINT32_MAX - 2);
    atomic_store(&ring->read_frame, UINT32_MAX - 2);
    for (uint32_t index = 0; index < RING_FRAMES / 3 + 1; ++index) push_input(ring, &input);
    if (fat_ring_dropped(ring) != 1) return 3;
    uint32_t received = 0;
    while (fat_ring_pop(ring, out, 4)) received += 4;
    if (received != RING_FRAMES) return 4;
    fat_ring_destroy(ring);

    format.mChannelsPerFrame = 4;
    format.mBytesPerFrame = format.mBytesPerPacket = 4;
    format.mFormatFlags |= kAudioFormatFlagIsNonInterleaved;
    ring = fat_ring_create(&format);
    if (!ring) return 5;
    float channels[] = {0.2f, 0.4f, 0.6f, 0.8f};
    struct { UInt32 count; AudioBuffer buffers[4]; } planar = {
        4, {{1, 4, &channels[0]}, {1, 4, &channels[1]}, {1, 4, &channels[2]}, {1, 4, &channels[3]}}
    };
    push_input(ring, (AudioBufferList *)&planar);
    if (fat_ring_pop(ring, out, 4) != 1 || fabsf(out[0] - 0.4f) > 1e-6f ||
        fabsf(out[1] - 0.6f) > 1e-6f) return 6;
    planar.count = 3;
    push_input(ring, (AudioBufferList *)&planar);
    if (!fat_ring_invalid(ring)) return 7;
    fat_ring_destroy(ring);
    format.mBitsPerChannel = 16;
    if (fat_format_valid(&format)) return 8;
    return 0;
}
