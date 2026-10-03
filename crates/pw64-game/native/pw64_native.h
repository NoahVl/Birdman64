/*
 * Force-included (-FI) before every decomp C file in the native build.
 * Keep this tiny: it runs ahead of <ultra64.h>, so it may only set up
 * host-ABI types and feature macros. See docs/notes/native-build.md.
 */
#ifndef PW64_NATIVE_H
#define PW64_NATIVE_H

/* Host size_t (64-bit; `unsigned long long` on Windows, `unsigned long` on
 * Linux). ultratypes.h and the IDO libc headers would otherwise typedef a
 * 32-bit one, which breaks calls into the host CRT (memcpy, ...). */
#ifndef _SIZE_T_DEF
#define _SIZE_T_DEF
#define _SIZE_T
typedef __SIZE_TYPE__ size_t;
#endif

/* The decomp's `long` is 32-bit (IDO; also MSVC/LLP64), but 64-bit on LP64
 * (Linux). The patched headers spell every ABI-relevant `long` (u32/s32 in
 * ultratypes.h, Mtx_t and friends in gbi.h) as PW64_LONG32: `long` where
 * that is 32-bit (Windows keeps its exact types), else `int`. */
#if __SIZEOF_LONG__ == 4
#define PW64_LONG32 long
#else
#define PW64_LONG32 int
#endif

#define PW64_NATIVE 1

/* Pointer-width helpers for the pointer patch set (native-build.md §2).
 * The exe, RDRAM window (0x80000000..) and every other address the C sees
 * lie below 4 GB (asserted by pw64_game::memmap), so a 32-bit int holds any
 * of them losslessly; the only hazard is sign extension of an s32 (`long`)
 * address >= 0x80000000 on its way back to a pointer.
 *   PW64_U32(p): pointer -> 32-bit N64-style address (lossless).
 *   PW64_PTR(a): 32-bit address (s32 or u32) -> pointer, zero-extended. */
#define PW64_U32(p) ((unsigned int)(__UINTPTR_TYPE__)(p))
#define PW64_PTR(a) ((void*)(__UINTPTR_TYPE__)(unsigned int)(a))

/* Endianness (native-build.md §3, task 6). ROM data is big-endian; scalars
 * read through uvMemRead/uvConsumeBytes(1/2/4) are already converted. Structs
 * copied raw get swapped in place right after the copy:
 *   PW64_SWAP(p, count, T, layout): `count` elements of sizeof(T) at p, each
 *   described by `layout` (`[N]c`, c = b keep / h 2-byte / w 4-byte /
 *   d 8-byte swap). pw64_swap (Rust, pw64-platform swap.rs) panics if the
 *   layout doesn't add up to sizeof(T). */
void pw64_swap(void* p, unsigned int count, unsigned int stride, const char* layout);
#define PW64_SWAP(p, count, T, layout) \
    pw64_swap((p), (unsigned int)(count), (unsigned int)sizeof(T), (layout))
/* Panics (Rust) with a message; for native helpers and patched checks. */
_Noreturn void pw64_fatal(const char* msg, unsigned int value);
/* One-line trace from patched C code (only prints with PW64_TRACE_OS).
 * `value` carries the packed state: fields OR'd / shifted into 32 bits. */
void pw64_log(const char* msg, unsigned int value);
#define PW64_LOGF(msg, v) pw64_log(msg, (unsigned int)(v))
/* PW64_START: boot straight into one test (pw64-platform start.rs); 1 once. */
int pw64_direct_start(int* file, int* cls, int* veh, int* test, int* pilot);
unsigned int pw64_rsp(void);
#define PW64_SWAP16(p, count) pw64_swap((p), (unsigned int)(count), 2, "h")
#define PW64_SWAP32(p, count) pw64_swap((p), (unsigned int)(count), 4, "w")
/* Hor+ widescreen (native/src/pw64_widescreen.c): output aspect (0 = 4:3)
 * culling-frustum x scale and display-list marker for a channel
 * (UnkStruct_80204D94*). */
extern float pw64_widescreen_aspect;
float pw64_widescreen_xscale(const void* chan);
void pw64_widescreen_tag(const void* chan, int on);
/* Fill screen (renderer.md "Fill screen"), same file: 0 = off (set by Rust
 * before boot), the per-frame "a filled world view was drawn" flag (reset
 * at the top of uvGfxBegin, graphics.c.patch) and the vertical
 * culling-frustum scale for a fill view (1 otherwise). */
extern int pw64_fill_screen;
extern int pw64_fill_frame;
float pw64_fill_yscale(const void* chan);
/* Flight-HUD anchors ('L'/'C'/'R' horizontal; 'T'/'M'/'B' vertical, fill
 * screen), same file. Text keeps both anchors of its print per message
 * (font.c.patch) and re-emits them at uvFontGenDlist. */
extern int pw64_hud_anchor;
void pw64_hud_emit_anchor(int anchor);
void pw64_hud_set_anchor(int anchor);
extern int pw64_hud_vanchor;
void pw64_hud_emit_vanchor(int anchor);
void pw64_hud_set_anchor2(int anchor, int vanchor);
void pw64_hud_anchor_xy(int x, int y);
/* Flight-HUD on/off ("PWH", same file, pw64-gfx interp::HUD_TAG_ON/OFF):
 * hud.c.patch brackets the HUD branch of hudMainRender with it; the
 * renderer only uses the tag when OLED care is on. */
void pw64_hud_tag(int on);
/* uvCopyFrameBuf (graphics.c.patch): copy the HLE renderer's framebuffer
 * target `src` into `dst` (N64 fb addresses; pw64-platform headless.rs). */
void pw64_fb_copy(unsigned int dst, unsigned int src);
/* snowDraw (snow.c.patch): the `count` RDRAM pixel indexes (y*320+x) the
 * CPU wrote with `color` into the previous, finished framebuffer `fb`
 * (N64 fb address; pw64-platform headless.rs). `u32` is
 * `unsigned PW64_LONG32` (ultratypes.h), so `idx` matches it exactly. */
void pw64_fb_pixels(unsigned int fb, const unsigned PW64_LONG32* idx, int count, unsigned int color);
/* Present tick (sched.c.patch; pw64-platform os/vi.rs, framerate.md):
 * register the scheduler's present message (`OSMesgQueue*`, `OSMesg`), and
 * whether present ticks (not retraces) latch swaps and start gfx. */
void pw64_present_set_event(void* mq, void* msg);
int pw64_present_active(void);

#endif
