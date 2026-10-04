// SPDX-License-Identifier: MIT
// Confluence VAIO: the layout shared between the driver and the engine.
// The engine side mirrors it independently; both pin the same offsets.
#pragma once

#include <stddef.h>

#define CONFLUENCE_VAIO_MAGIC             0x4F494156u   /* "VAIO" */
#define CONFLUENCE_VAIO_VERSION           1u
#define CONFLUENCE_VAIO_SAMPLE_RATE       48000u
#define CONFLUENCE_VAIO_CHANNELS          2u
#define CONFLUENCE_VAIO_BYTES_PER_FRAME   8u            /* 2 x 32-bit signed PCM */
#define CONFLUENCE_VAIO_HEADER_BYTES      4096u         /* the ring starts here */
#define CONFLUENCE_VAIO_MIN_CAPACITY      1024u
#define CONFLUENCE_VAIO_MAX_CAPACITY      65536u
#define CONFLUENCE_VAIO_MIN_TARGET        64u
#define CONFLUENCE_VAIO_ENGINE_TIMEOUT_MS 40

/* CTL_CODE(FILE_DEVICE_SOUND, 0x900, METHOD_OUT_DIRECT, FILE_READ_ACCESS | FILE_WRITE_ACCESS) */
#define CONFLUENCE_VAIO_IOCTL_ATTACH      0x001DE402u

#define CONFLUENCE_VAIO_DEVICE_NAME       L"\\Device\\ConfluenceVaio"
#define CONFLUENCE_VAIO_SYMLINK           L"\\DosDevices\\Global\\ConfluenceVaio"
#define CONFLUENCE_VAIO_USER_PATH         L"\\\\.\\ConfluenceVaio"

typedef struct CONFLUENCE_VAIO_HEADER
{
    unsigned int                Magic;            /* engine */
    unsigned int                Version;          /* engine */
    unsigned int                CapacityFrames;   /* engine: power of two */
    unsigned int                TargetFrames;     /* engine: most frames the driver queues */
    volatile unsigned int       Attached;         /* driver: 1 once it accepted the region */
    volatile unsigned int       Streaming;        /* driver: 1 while an app stream runs */
    volatile unsigned long long WriteFrames;      /* driver: frames put in the ring, ever */
    volatile unsigned long long ReadFrames;       /* engine: frames taken, ever */
    volatile unsigned long long EngineHeartbeat;  /* engine: +1 per engine block */
    volatile unsigned long long DriverTicks;      /* driver: +1 per copy */
    volatile unsigned long long FreeRunFrames;    /* driver: frames played without the engine */
} CONFLUENCE_VAIO_HEADER;

static_assert(offsetof(CONFLUENCE_VAIO_HEADER, Magic) == 0, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, Version) == 4, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, CapacityFrames) == 8, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, TargetFrames) == 12, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, Attached) == 16, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, Streaming) == 20, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, WriteFrames) == 24, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, ReadFrames) == 32, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, EngineHeartbeat) == 40, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, DriverTicks) == 48, "layout");
static_assert(offsetof(CONFLUENCE_VAIO_HEADER, FreeRunFrames) == 56, "layout");
static_assert(sizeof(CONFLUENCE_VAIO_HEADER) == 64, "layout");
static_assert((0x1D << 16 | 3 << 14 | 0x900 << 2 | 2) == CONFLUENCE_VAIO_IOCTL_ATTACH, "CTL_CODE");
