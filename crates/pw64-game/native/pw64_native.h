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
/* Split-screen HUD view ("PWS", same file, pw64-gfx interp::SPLIT_TAG):
 * hudMainRender brackets the HUD with the current player's view. */
void pw64_hud_split_tag(int on);
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
/* Split-screen (native/src/pw64_split.c): 0 = off, 1 = on (set by Rust
 * before boot, pw64_game::set_split). pw64_split_layout: 0 top/bottom, 1
 * left/right (Rust, any time); pw64_split_lr: the layout latched at test
 * start. The helpers live in camera.c.patch; they take the game's Camera
 * as void* because this header is force-included before camera.h exists.
 * pw64_split_out_aspect (pw64_widescreen.c): the output aspect a
 * left/right view extends into (0 = none: plain 4:3 halves). */
extern int pw64_split;
extern int pw64_split_layout;
extern int pw64_split_lr;
void pw64_split_viewport(void* camera, int i);
float pw64_split_out_aspect(void);
/* Pilot collisions (native/src/pw64_collide.c, splitscreen.md "Interactions
 * toggle"): 1 = the two pilots can crash into each other. Read at each test
 * start; Rust may set it any time (pw64_game::set_split_collide, also live
 * from the settings screen). */
extern int pw64_split_collide;
/* Split-screen players (native/src/pw64_players.c, splitscreen.md "M2"):
 * per-player state lives in the game's ordinary globals; only the current
 * player's copy is live. Registered regions are swapped on a switch
 * (live -> slot[cur], slot[i] -> live). Registration is explicit (no
 * constructors: the game module also builds as a dylib with our CRT
 * replacement) and runs from gameInit when pw64_players > 1. */
#define PW64_MAX_PLAYERS 2
extern int pw64_players;    /* 1; 2 with split-screen (gameInit: pw64_split ? 2 : 1) */
extern int pw64_player_cur; /* whose state is live in the globals */
/* Players in the current test (game.c.patch test setup): pw64_players for
 * a split-capable test, else 1. Gates every split path (1 = original). */
extern int pw64_level_players;
void pw64_player_register(void* addr, unsigned int size); /* current bytes -> every slot */
void pw64_player_switch(int i);                           /* no-op if i == cur */
void pw64_players_sync(void);                             /* live bytes -> every other slot */
#define PW64_REG(sym) pw64_player_register(&(sym), (unsigned int)sizeof(sym))
/* One per patched file that owns per-player statics; pw64_players_init
 * calls them all (gameInit, only if pw64_players > 1). */
void pw64_reg_game(void);
void pw64_reg_camera(void);
void pw64_reg_hud(void);
void pw64_reg_shadow(void);
void pw64_reg_hang_glider0(void);
void pw64_reg_hang_glider1(void);
void pw64_reg_hang_glider2(void);
void pw64_reg_rocket_belt(void);
void pw64_reg_rocket_belt_sound(void);
void pw64_reg_rocket_belt_poses(void); /* code_AE460.c */
void pw64_reg_rocket_belt_move(void);  /* code_AC1A0.c */
void pw64_reg_gyrocopter(void);        /* gyrocopter.c: rate trim */
void pw64_reg_gyrocopter_sound(void);  /* gyrocopter_sound.c */
void pw64_reg_gyrocopter_poses(void);  /* code_7C4C0.c */
void pw64_reg_gyrocopter_move(void);   /* code_7CF30.c */
void pw64_reg_jumble_hopper(void);     /* code_82B90.c */
void pw64_reg_birdman(void);        /* birdman0.c: camera-look toggle + rate state */
void pw64_reg_birdman_sound(void);  /* bmsound.c */
void pw64_reg_birdman_flutter(void); /* birdman1.c */
void pw64_reg_birdman_move(void);   /* birdman2.c */
void pw64_reg_cannonball(void);       /* cannonball.c: aim, scoring, round state */
void pw64_reg_cannonball_pilot(void); /* code_60020.c: pilot wobble */
void pw64_reg_skydiving(void);        /* skydiving.c: formation, scoring, countdown */
void pw64_reg_skydiving_drift(void);  /* code_BA190.c: rate drift hold */
void pw64_reg_skydiving_sound(void);  /* sdsound.c */
void pw64_players_init(void);
void pw64_reg_task(void); /* task.c: objective hit state (+ its modules) */
/* Per-player objective dobjs (kernel dobj.c.patch): owning player + 1 per
 * dobj, 0 = shared. Draw and collision loops skip another player's dobj. */
extern unsigned char pw64_dobj_owner[100];
#define PW64_DOBJ_OTHER(id) \
    (pw64_dobj_owner[id] != 0 &&pw64_dobj_owner[id] != pw64_player_cur + 1)
void pw64_dobj_claim(PW64_LONG32 objId);          /* owned by the current player */
PW64_LONG32 pw64_dobj_clone(PW64_LONG32 src);     /* copy for the current player */
/* Kernel controller reads (controller.c.patch): with 2+ players, pad 0
 * means "the current player's pad", so hardcoded-0 sites follow the swap. */
#define PW64_CONT(i) ((pw64_players > 1 && (i) == 0) ? pw64_player_cur : (i))
/* Split-screen sound events (kernel audio_event.c.patch): vehicle sound
 * handlers are owned by the player live when they registered. A post made
 * while pw64_evt_scope = i (>= 0) reaches only player i's handlers; an
 * unscoped one (-1) reaches all but muted players' (pw64_evt_mute bit i:
 * finished or respawning). pw64_evt_post_player posts in player i's scope. */
extern int pw64_evt_scope;
extern unsigned int pw64_evt_mute;
void pw64_evt_post_player(int i, int event);
/* Listener pose of player i > 0 (kernel audio_emitter.c.patch, a Mtx4F*):
 * positional emitters are heard from the nearest player's listener. */
void pw64_listener_set(int i, void* mat);
/* Split-screen free flight (game.c.patch, test_menu.c.patch): 1 while a
 * flight started from the test menu's Free flight item runs. */
extern int pw64_free_flight;
/* Our ASCII text in the game's 16-bit glyph strings (native/src/pw64_text.c):
 * glyphs + 0xFFE + -1 into out[max]; returns the glyph count. */
int pw64_text_encode(const char* ascii, short* out, int max);
int pw64_text_encode_style(const char* ascii, short* out, int max, int bold);
/* World display-list buffer length (kernel/graphics.c.patch): two world
 * passes per frame with split-screen on nearly double the command count. */
#define PW64_DL_LEN 16384

#endif
