// Shared-memory audio ring between LS50Stream.driver (inside coreaudiod) and ls50streamd.
//
// ls50streamd owns the region and hands it to the driver over XPC (mach service LS50STREAM_MACH_SERVICE).
// The driver writes the device's output mix; ls50streamd streams it to the speakers over HTTP (DLNA).
// Mirrored in Rust: engine/src/shm.rs. Bump LS50STREAM_SHM_VERSION on any layout change.
// Adapted from Limbo (limbo_shm.h).

#ifndef LS50STREAM_SHM_H
#define LS50STREAM_SHM_H

#include <stdint.h>

#define LS50STREAM_MACH_SERVICE "io.github.kaihagseth.ls50stream.audio"
#define LS50STREAM_SHM_MAGIC 0x54534344u /* "DCST" little-endian */
#define LS50STREAM_SHM_VERSION 1u
#define LS50STREAM_CHANNELS 2u
#define LS50STREAM_RING_FRAMES 65536u /* power of two; ~340 ms at 192 kHz */

typedef struct {
    uint32_t magic;
    uint32_t version;
    uint32_t channels;
    uint32_t ring_frames;
    /* Written by the driver */
    uint32_t sample_rate;        /* the device's nominal rate: 48000, 96000 or 192000 */
    uint32_t io_running;         /* 1 while the device is running */
    uint32_t generation;         /* incremented whenever the timeline below restarts */
    uint32_t io_frames;          /* size of the last IO cycle */
    uint64_t anchor_host_time;   /* mach time of sample 0 of the current timeline */
    double   host_ticks_per_frame;
    uint64_t write_end;          /* sample time just after the newest frame written */
    /* Written by ls50streamd */
    uint32_t latency_frames;     /* reported to CoreAudio as the device latency */
    /* Written by the driver */
    uint32_t attached;           /* set when the driver maps the region */
    /* Written by ls50streamd */
    uint32_t device_hidden;      /* 1 = remove the device (no speaker on the network), 0 = show it */
    uint32_t reserved[23];
} LS50StreamShmHeader;            /* 160 bytes; samples follow */

/* Samples: float32, interleaved stereo, frame for sample time S at index (S % ring_frames). */
#define LS50STREAM_SHM_SAMPLES_OFFSET 256u
#define LS50STREAM_SHM_SIZE (LS50STREAM_SHM_SAMPLES_OFFSET + LS50STREAM_RING_FRAMES * LS50STREAM_CHANNELS * 4u)

#endif
