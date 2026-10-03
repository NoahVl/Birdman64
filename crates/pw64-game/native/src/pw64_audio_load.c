/*
 * ROM-layout loaders for the libultra audio files (native-build.md §3, task 6).
 *
 * On the N64, `alSeqFileNew`/`alBnkfNew` (bnkf.c) patch file offsets into
 * pointers *in place*, inside a raw copy of the big-endian file. On a 64-bit
 * little-endian host the structs are wider (8-byte pointers) and byte-swapped,
 * so instead these deserialise the BE ROM layout into freshly allocated
 * host-layout structs, applying the same offset -> pointer rules:
 *   - bankArray/instArray/soundArray/envelope/keyMap/wavetable/book: file
 *     base + offset, no null check (instArray offset 0 would point at the
 *     file header; see below);
 *   - percussion, loop: 0 stays NULL;
 *   - wavetable base: table + offset (a ROM address, streamed by DMA).
 * Shared sub-structs stay shared (memo by file offset), as with in-place
 * patching. Wave data (.tbl) stays in ROM; ADPCM books/loops are converted.
 */
#include <ultra64.h>
#include <PR/libaudio.h>
#include <uv_memory.h>
#include <libc/stddef.h>
#include <pw64_endian.h>

/* Both ctl files (music 0x4070, SFX 0x67E8 bytes) fit. */
#define CTL_BUF_SIZE 0x8000
static u8 sCtlBuf[CTL_BUF_SIZE] __attribute__((aligned(16)));
static u32 sCtlSize;

#define MEMO_MAX 1024
static u32 sMemoOff[MEMO_MAX];
static void* sMemoPtr[MEMO_MAX];
static s32 sMemoCount;

static u8* sTable;

#define pw64_fail(what, value) pw64_fatal("pw64_audio_load: " what, (value))

static u32 rd8(u32 off) {
    if (off >= sCtlSize) pw64_fail("ctl read out of range", off);
    return sCtlBuf[off];
}
static u32 rd16(u32 off) { return (rd8(off) << 8) | rd8(off + 1); }
static u32 rd32(u32 off) { return (rd16(off) << 16) | rd16(off + 2); }

static void* memo_get(u32 off) {
    s32 i;
    for (i = 0; i < sMemoCount; i++) {
        if (sMemoOff[i] == off) return sMemoPtr[i];
    }
    return NULL;
}

static void memo_put(u32 off, void* p) {
    if (sMemoCount >= MEMO_MAX) pw64_fail("memo full", off);
    sMemoOff[sMemoCount] = off;
    sMemoPtr[sMemoCount] = p;
    sMemoCount++;
}

static void* alloc(s32 size) {
    /* 16-aligned like alHeapAlloc (ADPCM books must be 8-aligned). */
    return pw64_arena_alloc((u32)size, 16);
}

static ALEnvelope* load_env(u32 off) {
    ALEnvelope* e = memo_get(off);
    if (e != NULL) return e;
    e = alloc(sizeof(ALEnvelope));
    e->attackTime = (s32)rd32(off + 0);
    e->decayTime = (s32)rd32(off + 4);
    e->releaseTime = (s32)rd32(off + 8);
    e->attackVolume = rd8(off + 12);
    e->decayVolume = rd8(off + 13);
    memo_put(off, e);
    return e;
}

static ALKeyMap* load_keymap(u32 off) {
    ALKeyMap* k = memo_get(off);
    if (k != NULL) return k;
    k = alloc(sizeof(ALKeyMap));
    k->velocityMin = rd8(off + 0);
    k->velocityMax = rd8(off + 1);
    k->keyMin = rd8(off + 2);
    k->keyMax = rd8(off + 3);
    k->keyBase = rd8(off + 4);
    k->detune = (s8)rd8(off + 5);
    memo_put(off, k);
    return k;
}

static ALADPCMBook* load_book(u32 off) {
    ALADPCMBook* b = memo_get(off);
    s32 order, npred, n, i;
    if (b != NULL) return b;
    order = (s32)rd32(off + 0);
    npred = (s32)rd32(off + 4);
    n = order * npred * 8;
    b = alloc((s32)offsetof(ALADPCMBook, book) + n * 2);
    b->order = order;
    b->npredictors = npred;
    for (i = 0; i < n; i++) {
        b->book[i] = (s16)rd16(off + 8 + i * 2);
    }
    memo_put(off, b);
    return b;
}

static ALADPCMloop* load_adpcm_loop(u32 off) {
    ALADPCMloop* l = memo_get(off);
    s32 i;
    if (l != NULL) return l;
    l = alloc(sizeof(ALADPCMloop));
    l->start = rd32(off + 0);
    l->end = rd32(off + 4);
    l->count = rd32(off + 8);
    for (i = 0; i < ADPCMFSIZE; i++) {
        l->state[i] = (s16)rd16(off + 12 + i * 2);
    }
    memo_put(off, l);
    return l;
}

static ALRawLoop* load_raw_loop(u32 off) {
    ALRawLoop* l = memo_get(off);
    if (l != NULL) return l;
    l = alloc(sizeof(ALRawLoop));
    l->start = rd32(off + 0);
    l->end = rd32(off + 4);
    l->count = rd32(off + 8);
    memo_put(off, l);
    return l;
}

/* ROM ALWaveTable: u32 base, s32 len, u8 type, u8 flags, (2 pad), u32 loop,
 * u32 book. Mirrors _bnkfPatchWaveTable. */
static ALWaveTable* load_wave(u32 off) {
    ALWaveTable* w = memo_get(off);
    u32 loop;
    if (w != NULL) return w;
    w = alloc(sizeof(ALWaveTable));
    w->base = sTable + rd32(off + 0);
    w->len = (s32)rd32(off + 4);
    w->type = rd8(off + 8);
    w->flags = 1;
    loop = rd32(off + 12);
    if (w->type == AL_ADPCM_WAVE) {
        w->waveInfo.adpcmWave.book = load_book(rd32(off + 16));
        w->waveInfo.adpcmWave.loop = loop ? load_adpcm_loop(loop) : NULL;
    } else if (w->type == AL_RAW16_WAVE) {
        w->waveInfo.rawWave.loop = loop ? load_raw_loop(loop) : NULL;
    }
    memo_put(off, w);
    return w;
}

/* ROM ALSound: u32 envelope, u32 keyMap, u32 wavetable, u8 samplePan,
 * u8 sampleVolume, u8 flags. Mirrors _bnkfPatchSound. */
static ALSound* load_sound(u32 off) {
    ALSound* s = memo_get(off);
    if (s != NULL) return s;
    s = alloc(sizeof(ALSound));
    s->envelope = load_env(rd32(off + 0));
    s->keyMap = load_keymap(rd32(off + 4));
    s->wavetable = load_wave(rd32(off + 8));
    s->samplePan = rd8(off + 12);
    s->sampleVolume = rd8(off + 13);
    s->flags = 1;
    memo_put(off, s);
    return s;
}

/* Stands in for instArray slots with offset 0. On the N64, alBnkfNew adds the
 * base before its null check, so such a slot points at the file header read
 * as an instrument (flags byte = 1, so it is never patched; its soundCount is
 * the low half of the sample rate). Nothing sane can be reproduced from that:
 * give it no sounds, so notes on it find nothing to play. */
static ALInstrument sEmptyInst;

/* ROM ALInstrument: 12 x u8, s16 bendRange, s16 soundCount, u32 soundArray[].
 * Mirrors _bnkfPatchInst. */
static ALInstrument* load_inst(u32 off) {
    ALInstrument* inst;
    s32 n, i;
    if (off == 0) return &sEmptyInst;
    inst = memo_get(off);
    if (inst != NULL) return inst;
    n = (s16)rd16(off + 14);
    inst = alloc((s32)offsetof(ALInstrument, soundArray) + (n > 0 ? n : 1) * (s32)sizeof(ALSound*));
    inst->volume = rd8(off + 0);
    inst->pan = rd8(off + 1);
    inst->priority = rd8(off + 2);
    inst->flags = 1;
    inst->tremType = rd8(off + 4);
    inst->tremRate = rd8(off + 5);
    inst->tremDepth = rd8(off + 6);
    inst->tremDelay = rd8(off + 7);
    inst->vibType = rd8(off + 8);
    inst->vibRate = rd8(off + 9);
    inst->vibDepth = rd8(off + 10);
    inst->vibDelay = rd8(off + 11);
    inst->bendRange = (s16)rd16(off + 12);
    inst->soundCount = (s16)n;
    memo_put(off, inst);
    for (i = 0; i < n; i++) {
        inst->soundArray[i] = load_sound(rd32(off + 16 + i * 4));
    }
    return inst;
}

/* ROM ALBank: s16 instCount, u8 flags, u8 pad, s32 sampleRate,
 * u32 percussion, u32 instArray[]. Mirrors _bnkfPatchBank. */
static ALBank* load_bank(u32 off) {
    ALBank* bank = memo_get(off);
    s32 n, i;
    u32 perc;
    if (bank != NULL) return bank;
    n = (s16)rd16(off + 0);
    bank = alloc((s32)offsetof(ALBank, instArray) + (n > 0 ? n : 1) * (s32)sizeof(ALInstrument*));
    bank->instCount = (s16)n;
    bank->flags = 1;
    bank->pad = rd8(off + 3);
    bank->sampleRate = (s32)rd32(off + 4);
    memo_put(off, bank);
    perc = rd32(off + 8);
    bank->percussion = perc ? load_inst(perc) : NULL;
    for (i = 0; i < n; i++) {
        bank->instArray[i] = load_inst(rd32(off + 12 + i * 4));
    }
    return bank;
}

/* Replaces `_uvMediaCopy(file, src, size); alBnkfNew(file, table)`: reads the
 * BE .ctl at `src` (ROM offset or RAM, via _uvMediaCopy) and returns a
 * host-layout ALBankFile. The structs come from the native arena: widened,
 * they don't fit in the game's audio heap next to the (also wider) synth
 * state, and the N64 raw copy they replace is no longer allocated there. */
ALBankFile* pw64_alBnkfLoad(void* src, u32 size, u8* table) {
    ALBankFile* file;
    s32 n, i;

    if (size > CTL_BUF_SIZE) pw64_fail("ctl too big", size);
    _uvMediaCopy(sCtlBuf, src, size);
    sCtlSize = size;
    sMemoCount = 0;
    sTable = table;
    if (rd16(0) != AL_BANK_VERSION) pw64_fail("bad bank revision", rd16(0));
    n = (s16)rd16(2);
    file = alloc((s32)offsetof(ALBankFile, bankArray) + (n > 0 ? n : 1) * (s32)sizeof(ALBank*));
    file->revision = (s16)rd16(0);
    file->bankCount = (s16)n;
    for (i = 0; i < n; i++) {
        /* On the N64 an offset of 0 would become the header; unused here. */
        file->bankArray[i] = load_bank(rd32(4 + i * 4));
    }
    return file;
}

/* Replaces the raw ALSeqFile header copies in uvaManagerInit: reads the BE
 * file at `src` (ROM: u16 revision, u16 seqCount, {u32 offset, s32 len}[])
 * into a host-layout ALSeqFile from `heap`. Offsets stay file-relative;
 * alSeqFileNew adds the base as on the N64. */
ALSeqFile* pw64_alSeqFileLoad(void* src, ALHeap* heap) {
    u8 hdr[8];
    u8* buf;
    ALSeqFile* file;
    s32 n, i, bytes;

    _uvMediaCopy(hdr, src, sizeof(hdr));
    n = (s16)((hdr[2] << 8) | hdr[3]);
    bytes = 4 + n * 8;
    file = alHeapAlloc(heap, 1, (s32)offsetof(ALSeqFile, seqArray) + n * (s32)sizeof(ALSeqData));
    if (file == NULL) pw64_fail("audio heap full (seq file)", (u32)n);
    /* Stage the BE table in the ctl buffer (unused at this point). */
    if (bytes > CTL_BUF_SIZE) pw64_fail("seq table too big", (u32)bytes);
    _uvMediaCopy(sCtlBuf, src, (bytes + 1) & ~1);
    buf = sCtlBuf;
    file->revision = (s16)((buf[0] << 8) | buf[1]);
    file->seqCount = (s16)n;
    for (i = 0; i < n; i++) {
        u8* e = buf + 4 + i * 8;
        u32 off = ((u32)e[0] << 24) | ((u32)e[1] << 16) | ((u32)e[2] << 8) | e[3];
        file->seqArray[i].offset = PW64_PTR(off);
        file->seqArray[i].len = (s32)(((u32)e[4] << 24) | ((u32)e[5] << 16) | ((u32)e[6] << 8) | e[7]);
    }
    return file;
}
