// SPDX-License-Identifier: MIT
// Confluence VAIO: moving played audio from the WaveRT cyclic buffer into the
// engine's ring. Plain C++ with no kernel headers, so user-mode tests can run
// it; the kernel calls it under its link spinlock.
//
// Nothing in the shared region is trusted after attach: the capacity and
// target are copied and validated then, and the driver keeps its own count
// of frames written. Engine counters that make no sense mean "no room".
#pragma once

#include <intrin.h>
#include "confluence_vaio_abi.h"

namespace vaio {

inline unsigned long long load64(volatile unsigned long long* p)
{
    return (unsigned long long)_InterlockedOr64((volatile long long*)p, 0);
}

inline void store64(volatile unsigned long long* p, unsigned long long v)
{
    _InterlockedExchange64((volatile long long*)p, (long long)v);
}

inline void store32(volatile unsigned int* p, unsigned int v)
{
    _InterlockedExchange((volatile long*)p, (long)v);
}

struct Link
{
    CONFLUENCE_VAIO_HEADER* header;          // nullptr while detached
    unsigned char*          ring;
    unsigned int            capacity;        // frames, validated at attach
    unsigned int            target;          // frames, validated at attach
    unsigned long long      written;         // the driver's own count
    unsigned long long      last_heartbeat;
    long long               heartbeat_changed_ms;
};

// Validates an engine region of `bytes` bytes and takes it over.
inline bool attach(Link& l, void* region, unsigned long long bytes, long long now_ms)
{
    if (region == nullptr || bytes < CONFLUENCE_VAIO_HEADER_BYTES)
    {
        return false;
    }
    CONFLUENCE_VAIO_HEADER* h = (CONFLUENCE_VAIO_HEADER*)region;
    unsigned int capacity = h->CapacityFrames;
    unsigned int target = h->TargetFrames;
    bool pow2 = capacity != 0 && (capacity & (capacity - 1)) == 0;
    if (h->Magic != CONFLUENCE_VAIO_MAGIC || h->Version != CONFLUENCE_VAIO_VERSION || !pow2
        || capacity < CONFLUENCE_VAIO_MIN_CAPACITY || capacity > CONFLUENCE_VAIO_MAX_CAPACITY
        || target < CONFLUENCE_VAIO_MIN_TARGET || target > capacity / 2
        || bytes < CONFLUENCE_VAIO_HEADER_BYTES + (unsigned long long)capacity * CONFLUENCE_VAIO_BYTES_PER_FRAME)
    {
        return false;
    }
    l.header = h;
    l.ring = (unsigned char*)region + CONFLUENCE_VAIO_HEADER_BYTES;
    l.capacity = capacity;
    l.target = target;
    l.written = 0;
    l.last_heartbeat = load64(&h->EngineHeartbeat);
    l.heartbeat_changed_ms = now_ms;
    store64(&h->WriteFrames, 0);
    store32(&h->Attached, 1);
    return true;
}

inline void detach(Link& l)
{
    l = Link{};
}

// How long the engine may stay silent before the stream free-runs: the base
// timeout, or three ring targets' worth for long engine blocks (the engine's
// heartbeat moves once per block).
inline long long engine_timeout_ms(const Link& l)
{
    long long ring_ms = (long long)l.target * 3 * 1000 / CONFLUENCE_VAIO_SAMPLE_RATE;
    return ring_ms > CONFLUENCE_VAIO_ENGINE_TIMEOUT_MS ? ring_ms : CONFLUENCE_VAIO_ENGINE_TIMEOUT_MS;
}

// While the engine drives, the stream may run at most 33/32 of real time
// (whole frames). The ring's room decides how far it actually goes, so a slow
// engine still throttles it, and the ~3% headroom covers an engine clock that
// runs fast. It never jumps a whole engine block at once, and never gets far
// ahead of what the app has written: both read stale audio and miscount
// packets (a 2048-frame block refilled at 1.25x still clicked).
inline unsigned int pace(unsigned int time_bytes)
{
    unsigned int frames = time_bytes / CONFLUENCE_VAIO_BYTES_PER_FRAME;
    return frames * 33 / 32 * CONFLUENCE_VAIO_BYTES_PER_FRAME;
}

// True while the engine's heartbeat moved within the timeout.
inline bool engine_alive(Link& l, long long now_ms)
{
    if (l.header == nullptr)
    {
        return false;
    }
    unsigned long long hb = load64(&l.header->EngineHeartbeat);
    if (hb != l.last_heartbeat)
    {
        l.last_heartbeat = hb;
        l.heartbeat_changed_ms = now_ms;
    }
    return now_ms - l.heartbeat_changed_ms <= engine_timeout_ms(l);
}

// Frames queued for the engine, or `capacity` when its counter makes no sense.
inline unsigned int queued(const Link& l)
{
    unsigned long long read = load64(&l.header->ReadFrames);
    unsigned long long fill = l.written - read;   // wraps to huge if read > written
    return fill > l.capacity ? l.capacity : (unsigned int)fill;
}

// Bytes (whole frames) the stream may advance while the engine drives it:
// what brings the ring up to the target, at most `max_bytes`.
inline unsigned int plan(const Link& l, unsigned int max_bytes)
{
    if (l.header == nullptr)
    {
        return 0;
    }
    unsigned int fill = queued(l);
    unsigned int room = fill < l.target ? l.target - fill : 0;
    unsigned int frames = max_bytes / CONFLUENCE_VAIO_BYTES_PER_FRAME;
    if (frames > room)
    {
        frames = room;
    }
    return frames * CONFLUENCE_VAIO_BYTES_PER_FRAME;
}

// Copies `bytes` from the cyclic buffer (`src`, `src_size` bytes, starting at
// `src_offset`) into the ring, clamped to what plan() allows.
inline void copy(Link& l, const unsigned char* src, unsigned int src_size, unsigned int src_offset, unsigned int bytes)
{
    if (l.header == nullptr || src == nullptr || src_size == 0)
    {
        return;
    }
    unsigned int allowed = plan(l, bytes);
    unsigned int frames = allowed / CONFLUENCE_VAIO_BYTES_PER_FRAME;
    unsigned int from = src_offset % src_size;
    for (unsigned int n = 0; n < frames; ++n)
    {
        unsigned int to = (unsigned int)(l.written % l.capacity) * CONFLUENCE_VAIO_BYTES_PER_FRAME;
        for (unsigned int b = 0; b < CONFLUENCE_VAIO_BYTES_PER_FRAME; ++b)
        {
            l.ring[to + b] = src[(from + b) % src_size];
        }
        from = (from + CONFLUENCE_VAIO_BYTES_PER_FRAME) % src_size;
        ++l.written;
    }
    store64(&l.header->WriteFrames, l.written);
    store64(&l.header->DriverTicks, load64(&l.header->DriverTicks) + 1);
}

inline void note_free_run(Link& l, unsigned int bytes)
{
    if (l.header != nullptr)
    {
        unsigned long long f = load64(&l.header->FreeRunFrames);
        store64(&l.header->FreeRunFrames, f + bytes / CONFLUENCE_VAIO_BYTES_PER_FRAME);
    }
}

inline void set_streaming(Link& l, bool on)
{
    if (l.header != nullptr)
    {
        store32(&l.header->Streaming, on ? 1u : 0u);
    }
}

} // namespace vaio
