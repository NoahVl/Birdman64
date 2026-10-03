/*
 * Native-only ROM-layout loaders (native-build.md §3, task 6), for ROM
 * structs whose host layout differs from the N64 one (pointer fields), so a
 * byte swap in place (PW64_SWAP, pw64_native.h) isn't enough.
 * Included by the patched decomp files that call them.
 */
#ifndef PW64_ENDIAN_H
#define PW64_ENDIAN_H

#include <ultra64.h>
#include <PR/libaudio.h>

/* native/src/pw64_arena.c: zeroed bump allocation in the RDRAM window
 * (0x802000A0..0x80380000, the unused N64 code area). Never freed. */
void* pw64_arena_alloc(u32 size, u32 align);

/* native/src/pw64_audio_load.c. The ALSeqFile comes from `heap` (as on the
 * N64); the bank structs from pw64_arena_alloc (they don't fit the heap). */
ALBankFile* pw64_alBnkfLoad(void* src, u32 size, u8* table);
ALSeqFile* pw64_alSeqFileLoad(void* src, ALHeap* heap);

/* native/src/pw64_rom_structs.c: BE N64-layout records in RAM -> host. */
struct bitmap;
#define PW64_N64_BITMAP_SIZE 16
void pw64_load_bitmaps(struct bitmap* dst, const void* src, s32 count);

#endif
