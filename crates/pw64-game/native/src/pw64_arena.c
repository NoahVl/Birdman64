/*
 * Native-only bump arena inside the RDRAM window, for host data that has no
 * N64 counterpart or is bigger on the host (8-byte pointers), e.g. the
 * deserialised audio banks (pw64_audio_load.c), which don't fit in the
 * game's fixed 0x413DC-byte audio heap once widened.
 *
 * Range: 0x80200000..0x80380000, the N64 kernel+app image area. The game
 * reserves it from its own heaps (memory.c uvMemInitBlocks, block 4) but on
 * the host the code/data live in the exe image, so it is free. Being in the
 * window matters: the audio lib passes ADPCM books/loop states through
 * K0_TO_PHYS (29-bit mask), which only window addresses survive.
 *
 * Starts at 0x802000A0 (kernel_TEXT_START), not 0x80200000: every level load
 * runs memory.c uvMemClearRegions, which zeroes 0x80125800..kernel_TEXT_START.
 * With the arena at 0x80200000 that wiped the music bank's header (instCount,
 * sampleRate, percussion, instArray[0..15]) after it was loaded, so the seq
 * player ignored every program change and played all channels with
 * instArray[16] (audio.md "B13").
 */
#include <ultra64.h>
#include <pw64_endian.h>

#define ARENA_START 0x802000A0u
#define ARENA_END 0x80380000u

static u32 sArenaCur = ARENA_START;

void* pw64_arena_alloc(u32 size, u32 align) {
    u32 p = (sArenaCur + (align - 1)) & ~(align - 1);
    u8* q;
    u32 i;
    if (p + size > ARENA_END || p + size < p) {
        pw64_fatal("pw64_arena_alloc: native arena full", size);
    }
    sArenaCur = p + size;
    q = PW64_PTR(p);
    for (i = 0; i < size; i++) {
        q[i] = 0;
    }
    return q;
}
