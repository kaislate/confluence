// SPDX-License-Identifier: MIT
// User-mode tests for vaio_pump.h (the driver's copy/advance logic).
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>
#include "../Source/Inc/vaio_pump.h"

static int g_failures = 0;
#define CHECK(c) do { if (!(c)) { std::printf("  FAIL %s:%d: %s\n", __FILE__, __LINE__, #c); ++g_failures; } } while (0)

// An engine-side region as the engine lays it out.
struct Region {
    std::vector<unsigned char> bytes;
    Region(unsigned int capacity, unsigned int target, unsigned int magic = CONFLUENCE_VAIO_MAGIC)
        : bytes(CONFLUENCE_VAIO_HEADER_BYTES + (size_t)capacity * CONFLUENCE_VAIO_BYTES_PER_FRAME) {
        auto* h = header();
        h->Magic = magic;
        h->Version = CONFLUENCE_VAIO_VERSION;
        h->CapacityFrames = capacity;
        h->TargetFrames = target;
    }
    CONFLUENCE_VAIO_HEADER* header() { return reinterpret_cast<CONFLUENCE_VAIO_HEADER*>(bytes.data()); }
    float* ring() { return reinterpret_cast<float*>(bytes.data() + CONFLUENCE_VAIO_HEADER_BYTES); }
};

// A cyclic buffer whose frame n holds (n, -n).
static std::vector<unsigned char> cyclic(unsigned int frames) {
    std::vector<unsigned char> b(frames * 8);
    for (unsigned int n = 0; n < frames; ++n) {
        float s[2] = { (float)n, -(float)n };
        std::memcpy(&b[n * 8], s, 8);
    }
    return b;
}

static void rejects_regions_it_cannot_trust() {
    vaio::Link l{};
    Region ok(1024, 256);
    CHECK(vaio::attach(l, ok.bytes.data(), ok.bytes.size(), 0));
    CHECK(ok.header()->Attached == 1);
    Region bad_magic(1024, 256, 0x12345678);
    CHECK(!vaio::attach(l, bad_magic.bytes.data(), bad_magic.bytes.size(), 0));
    Region not_pow2(1000, 256);
    CHECK(!vaio::attach(l, not_pow2.bytes.data(), not_pow2.bytes.size(), 0));
    Region too_small_cap(512, 128);
    CHECK(!vaio::attach(l, too_small_cap.bytes.data(), too_small_cap.bytes.size(), 0));
    Region target_too_big(1024, 600);
    CHECK(!vaio::attach(l, target_too_big.bytes.data(), target_too_big.bytes.size(), 0));
    Region target_too_small(1024, 16);
    CHECK(!vaio::attach(l, target_too_small.bytes.data(), target_too_small.bytes.size(), 0));
    Region short_buffer(1024, 256);
    CHECK(!vaio::attach(l, short_buffer.bytes.data(), short_buffer.bytes.size() - 8, 0));
    CHECK(!vaio::attach(l, nullptr, 0, 0));
    // A misaligned region would make every interlocked access a split lock.
    std::vector<unsigned char> raw(CONFLUENCE_VAIO_HEADER_BYTES + 1024 * 8 + 8);
    Region proto(1024, 256);
    std::memcpy(raw.data() + 1, proto.bytes.data(), CONFLUENCE_VAIO_HEADER_BYTES);
    CHECK(!vaio::attach(l, raw.data() + 1, raw.size() - 1, 0));
}

static void fills_the_ring_up_to_target_and_no_further() {
    Region r(1024, 256);
    vaio::Link l{};
    CHECK(vaio::attach(l, r.bytes.data(), r.bytes.size(), 0));
    auto src = cyclic(480);
    CHECK(vaio::engine_alive(l, 0));
    unsigned int bytes = vaio::plan(l, 480 * 8);
    CHECK(bytes == 256 * 8);
    vaio::copy(l, src.data(), (unsigned int)src.size(), 0, bytes);
    CHECK(r.header()->WriteFrames == 256);
    CHECK(r.ring()[0] == 0.0f && r.ring()[1] == -0.0f);
    CHECK(r.ring()[255 * 2] == 255.0f && r.ring()[255 * 2 + 1] == -255.0f);
    CHECK(vaio::plan(l, 480 * 8) == 0);            // full to target
    r.header()->ReadFrames = 100;                  // the engine took 100
    CHECK(vaio::plan(l, 480 * 8) == 100 * 8);
}

static void wraps_in_both_buffers() {
    Region r(1024, 900 > 512 ? 512 : 900);
    vaio::Link l{};
    CHECK(vaio::attach(l, r.bytes.data(), r.bytes.size(), 0));
    auto src = cyclic(100);
    // Start 10 frames before the end of the cyclic buffer, and 10 frames before the end of the ring.
    l.written = 1014;
    r.header()->WriteFrames = 1014;
    r.header()->ReadFrames = 1014;
    vaio::copy(l, src.data(), (unsigned int)src.size(), 90 * 8, 20 * 8);
    CHECK(r.header()->WriteFrames == 1034);
    CHECK(r.ring()[1014 * 2] == 90.0f);            // src frame 90 at ring frame 1014
    CHECK(r.ring()[1023 * 2] == 99.0f);            // src frame 99 at the ring's last frame
    CHECK(r.ring()[0] == 0.0f && r.ring()[9 * 2] == 9.0f); // then src 0..9 at ring 0..9
}

static void plans_whole_frames_within_the_limit() {
    Region r(1024, 256);
    vaio::Link l{};
    CHECK(vaio::attach(l, r.bytes.data(), r.bytes.size(), 0));
    CHECK(vaio::plan(l, 100 * 8 + 5) == 100 * 8);  // never a partial frame
    CHECK(vaio::plan(l, 3) == 0);
}

static void a_silent_engine_hands_the_clock_back() {
    Region r(1024, 256);
    vaio::Link l{};
    CHECK(vaio::attach(l, r.bytes.data(), r.bytes.size(), 1000));
    CHECK(vaio::engine_alive(l, 1000 + CONFLUENCE_VAIO_ENGINE_TIMEOUT_MS));
    CHECK(!vaio::engine_alive(l, 1000 + CONFLUENCE_VAIO_ENGINE_TIMEOUT_MS + 1));
    r.header()->EngineHeartbeat = 1;               // the engine ran a block
    CHECK(vaio::engine_alive(l, 5000));
    CHECK(!vaio::engine_alive(l, 5000 + CONFLUENCE_VAIO_ENGINE_TIMEOUT_MS + 1));
    vaio::note_free_run(l, 480 * 8);
    CHECK(r.header()->FreeRunFrames == 480);
}

static void garbage_counters_never_overflow_the_ring() {
    Region r(1024, 256);
    vaio::Link l{};
    CHECK(vaio::attach(l, r.bytes.data(), r.bytes.size(), 0));
    r.header()->ReadFrames = 999999;               // ahead of anything written
    CHECK(vaio::plan(l, 480 * 8) == 0);
    r.header()->ReadFrames = 0;
    l.written = 5000;                              // more than a ring's worth unread
    CHECK(vaio::plan(l, 480 * 8) == 0);
    // Capacity and target changed after attach are ignored: the copies taken at attach rule.
    l.written = 0;
    r.header()->CapacityFrames = 1u << 30;
    r.header()->TargetFrames = 1u << 29;
    CHECK(vaio::plan(l, 480 * 8) == 256 * 8);
    auto src = cyclic(480);
    vaio::copy(l, src.data(), (unsigned int)src.size(), 0, 480 * 8); // asks for more than planned
    CHECK(r.header()->WriteFrames <= 1024);        // never beyond one ring's worth
}

static void detached_links_do_nothing() {
    vaio::Link l{};
    CHECK(vaio::plan(l, 4096) == 0);
    auto src = cyclic(16);
    vaio::copy(l, src.data(), (unsigned int)src.size(), 0, 64); // must not crash
    vaio::note_free_run(l, 64);
    vaio::set_streaming(l, true);
    Region r(1024, 256);
    CHECK(vaio::attach(l, r.bytes.data(), r.bytes.size(), 0));
    vaio::set_streaming(l, true);
    CHECK(r.header()->Streaming == 1);
    vaio::detach(l);
    CHECK(l.header == nullptr && vaio::plan(l, 4096) == 0);
}

// Engine-driven, the stream may run at most 33/32 of real time: never a
// whole engine block in one tick, and never far ahead of what the app wrote
// (a long engine block refilled at 1.25x read stale audio).
static void the_engine_driven_pace_is_close_to_real_time() {
    CHECK(vaio::pace(480 * 8) == 495 * 8);         // 10 ms of time: at most ~10.3 ms of audio
    CHECK(vaio::pace(48 * 8) == 49 * 8);           // one 1 ms tick: 49 frames, so it can catch up
    CHECK(vaio::pace(8) == 8);                     // whole frames only
    CHECK(vaio::pace(7) == 0);
    CHECK(vaio::pace(0) == 0);
}

// Long engine blocks: the engine's heartbeat moves once per block, so the
// timeout must outlast a block (three ring targets, never below the base).
static void the_engine_timeout_outlasts_long_blocks() {
    Region small(1024, 448);                       // block 256 + 192
    vaio::Link l{};
    CHECK(vaio::attach(l, small.bytes.data(), small.bytes.size(), 0));
    CHECK(vaio::engine_timeout_ms(l) == CONFLUENCE_VAIO_ENGINE_TIMEOUT_MS);
    Region big(8192, 2240);                        // block 2048 + 192 (46.7 ms)
    vaio::Link b{};
    CHECK(vaio::attach(b, big.bytes.data(), big.bytes.size(), 1000));
    CHECK(vaio::engine_timeout_ms(b) == 140);      // 3 x 2240 frames at 48 kHz
    CHECK(vaio::engine_alive(b, 1000 + 100));
    CHECK(vaio::engine_alive(b, 1000 + 140));
    CHECK(!vaio::engine_alive(b, 1000 + 141));
}

int main() {
    the_engine_driven_pace_is_close_to_real_time();
    the_engine_timeout_outlasts_long_blocks();
    rejects_regions_it_cannot_trust();
    fills_the_ring_up_to_target_and_no_further();
    wraps_in_both_buffers();
    plans_whole_frames_within_the_limit();
    a_silent_engine_hands_the_clock_back();
    garbage_counters_never_overflow_the_ring();
    detached_links_do_nothing();
    if (g_failures) { std::printf("%d check(s) failed\n", g_failures); return 1; }
    std::printf("all pump tests passed\n");
    return 0;
}
