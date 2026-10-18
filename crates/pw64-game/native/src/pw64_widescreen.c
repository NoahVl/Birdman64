/*
 * Hor+ widescreen (PW64_WIDESCREEN, renderer.md "Widescreen") and fill
 * screen (PW64_FILL_SCREEN, renderer.md "Fill screen"): the projection is
 * unchanged, the HLE renderer draws the world view's perspective geometry
 * past the 4:3 edges and its letterbox rows. What the C side must do:
 *  - cull with a frustum as wide (and, on a fill view, as tall) as the
 *    shown view (code_7150.c func_802061A0: object planes + terrain
 *    footprint), else objects and terrain pop at the new edges;
 *  - tag the world view in the display list (chan.c uvChan_80204FE4), so
 *    the renderer extends only that: a model shown over a 2D menu (pilot
 *    select) must stay inside its 4:3 background;
 *  - on a fill view, tell the border bars (drawScreenBorder, level_select)
 *    not to draw this frame (pw64_fill_frame, reset per uvGfxBegin).
 */
#include <ultra64.h>
#include <uv_graphics.h>

/* Output aspect w/h, 0 = 4:3 (off). Set by Rust before boot
 * (pw64_game::set_widescreen_aspect). */
float pw64_widescreen_aspect = 0.0f;

/* Fill screen, 0 = off. Set by Rust before boot
 * (pw64_game::set_fill_screen). */
int pw64_fill_screen = 0;

/* 1 once a filled world view was drawn this frame; reset at the top of
 * uvGfxBegin (graphics.c.patch), read by the border bars. */
int pw64_fill_frame = 0;

/* Must match pw64-gfx `interp::WIDE_TAG_ON/OFF` (G_NOOP w1). */
#define PW64_WIDE_TAG_ON 0x50575731u  /* "PWW1" */
#define PW64_WIDE_TAG_OFF 0x50575730u /* "PWW0" */

/* A channel that draws the 3D world: terrain (unk0 bit 2) or an
 * environment (unk2), over almost the full screen width. */
static int pw64_world_view(const UnkStruct_80204D94* c) {
    return (c->unk0 & 2 || c->unk2 != 0xFFFF) && c->viewX0 <= 16 &&
           c->viewX1 >= SCREEN_WIDTH - 16;
}

/* A fill view: fill on, a world view and a viewport covering the
 * letterboxed rows (bottom-up; the flight view is 18..232). 2D screens'
 * channels never match, so their bars stay. */
static int pw64_fill_view(const UnkStruct_80204D94* c) {
    return pw64_fill_screen && pw64_world_view(c) && c->viewY0 <= 20 &&
           c->viewY1 >= 220;
}

/* Horizontal culling-frustum scale for channel `chan`. Widened: a world
 * view (terrain or environment) whose viewport is the main view (full width
 * or the 10..310 flight subscreen; same bounds as pw64-gfx `Wide::MAIN_VIEW`),
 * shown 240*aspect N64 pixels wide. Other channels stay 4:3.
 * Fill screen without widescreen still shows the world past the subscreen
 * columns (the renderer extends its world draws to the full output), so the
 * effective aspect is 4:3 there.
 * +2% margin for the rounding of the output area. */
float pw64_widescreen_xscale(const void* chan) {
    const UnkStruct_80204D94* c = chan;
    float aspect = pw64_widescreen_aspect;
    float k;
    if (aspect <= 0.0f) {
        if (!pw64_fill_view(c)) {
            return 1.0f;
        }
        aspect = 4.0f / 3.0f;
    }
    if (c->viewX0 > 16 || c->viewX1 < SCREEN_WIDTH - 16 ||
        c->viewX1 <= c->viewX0 || (!(c->unk0 & 2) && c->unk2 == 0xFFFF)) {
        return 1.0f;
    }
    k = 240.0f * aspect / (float)(c->viewX1 - c->viewX0) * 1.02f;
    return k > 1.0f ? k : 1.0f;
}

/* Vertical culling-frustum scale for channel `chan` on a fill view, 1
 * otherwise. The guard viewport (uvGfxClipRect: the view +/- 5 px, clamped
 * to the screen) is centred at top-down row SCREEN_HEIGHT - g0 - a/2 with
 * NDC -1/+1 = a/2 rows (a = g1 - g0); the frustum must reach the guard row
 * farthest from that centre. Without fill the old view is symmetric about
 * row 115 and the game's frustum already reaches both ends.
 * +2% margin like the x scale. */
float pw64_fill_yscale(const void* chan) {
    const UnkStruct_80204D94* c = chan;
    int g0, g1, half, centre, hi;
    float k;
    if (!pw64_fill_view(c)) {
        return 1.0f;
    }
    g0 = c->viewY0 - 5;
    if (g0 < 0) {
        g0 = 0;
    }
    g1 = c->viewY1 + 5;
    if (g1 > SCREEN_HEIGHT - 1) {
        g1 = SCREEN_HEIGHT - 1;
    }
    half = (g1 - g0) / 2;
    centre = SCREEN_HEIGHT - g0 - half;
    hi = centre > SCREEN_HEIGHT - centre ? centre : SCREEN_HEIGHT - centre;
    k = (float)hi / (float)half * 1.02f;
    return k > 1.0f ? k : 1.0f;
}

/* Output aspect a left/right split view extends into: the widescreen
 * aspect, 4:3 with fill screen alone (the views reach the 4:3 edges), 0
 * when neither (the views stay inside the subscreen). Must match the
 * renderer's split world placement and pw64_split_viewport's frustum. */
float pw64_split_out_aspect(void) {
    if (pw64_widescreen_aspect > 0.0f) {
        return pw64_widescreen_aspect;
    }
    return pw64_fill_screen ? 4.0f / 3.0f : 0.0f;
}

/* Must match pw64-gfx `interp::SPLIT_TAG` ("PWS" + 'X'/'Y'/'0'). */
#define PW64_SPLIT_TAG 0x50575300u

/* The split-view tag pair for channel `c`'s viewport (top-down rows). */
static void pw64_split_rect_tag(const UnkStruct_80204D94* c) {
    gGfxDisplayListHead->words.w0 = ((u32)G_NOOP << 24) | ((u32)c->viewX0 << 12) | (u32)c->viewX1;
    gGfxDisplayListHead->words.w1 = PW64_SPLIT_TAG | 'X';
    gGfxDisplayListHead++;
    gGfxDisplayListHead->words.w0 = ((u32)G_NOOP << 24) | ((u32)(SCREEN_HEIGHT - c->viewY1) << 12) |
                                    (u32)(SCREEN_HEIGHT - c->viewY0);
    gGfxDisplayListHead->words.w1 = PW64_SPLIT_TAG | 'Y';
    gGfxDisplayListHead++;
}

static void pw64_split_off_tag(void) {
    gGfxDisplayListHead->words.w0 = (u32)G_NOOP << 24;
    gGfxDisplayListHead->words.w1 = PW64_SPLIT_TAG | '0';
    gGfxDisplayListHead++;
}

/* A left/right split world view (not full width) on a wider output: the
 * renderer stretches its draws from the outer subscreen edge to the
 * output's edge (pw64-gfx split world placement; the frustum is that wide,
 * pw64_split_viewport). The full-screen results view is a plain world
 * view. Culling follows the camera's own (already wide) frustum. */
static int pw64_split_world_view(const UnkStruct_80204D94* c) {
    return pw64_level_players > 1 && pw64_split_lr && pw64_split_out_aspect() > 0.0f &&
           (c->unk0 & 2 || c->unk2 != 0xFFFF) &&
           !(c->viewX0 <= 16 && c->viewX1 >= SCREEN_WIDTH - 16);
}

/* Brackets a channel's draw (on = 1 at the start, 0 at the end) with
 * G_NOOP markers when it is a widened world view or a fill view. RDP NOOPs
 * are harmless on hardware. A fill view marks the frame: the border bars
 * stay off until the next uvGfxBegin. A left/right split world view gets
 * its view rect too (the split tag outside a HUD bracket). */
void pw64_widescreen_tag(const void* chan, int on) {
    if (pw64_split_world_view(chan)) {
        if (!on) {
            pw64_split_off_tag();
        }
        gGfxDisplayListHead->words.w0 = (u32)G_NOOP << 24;
        gGfxDisplayListHead->words.w1 = on ? PW64_WIDE_TAG_ON : PW64_WIDE_TAG_OFF;
        gGfxDisplayListHead++;
        if (on) {
            pw64_split_rect_tag(chan);
        }
        return;
    }
    if (pw64_widescreen_xscale(chan) <= 1.0f && !pw64_fill_view(chan)) {
        return;
    }
    if (on && pw64_fill_view(chan)) {
        pw64_fill_frame = 1;
    }
    gGfxDisplayListHead->words.w0 = (u32)G_NOOP << 24;
    gGfxDisplayListHead->words.w1 = on ? PW64_WIDE_TAG_ON : PW64_WIDE_TAG_OFF;
    gGfxDisplayListHead++;
}

/*
 * Flight HUD edge anchoring. hud.c calls pw64_hud_anchor_x(x) at the top of
 * each gauge helper; the renderer moves the following 2D draws by the side
 * margin (pw64-gfx `Anchor`). Text is queued (uvFontPrintStr) and drawn at
 * uvFontGenDlist, so font.c stores the anchor current at print time per
 * message and re-emits it (pw64_hud_emit_anchor) when drawing.
 */

/* Must match pw64-gfx `interp::ANCHOR_TAG` ("PWA" + 'L'/'C'/'R'). */
#define PW64_ANCHOR_TAG 0x50574100u

/* Anchor for text printed now ('C' outside the flight HUD). */
int pw64_hud_anchor = 'C';

/* Vertical anchor for the following draws, used by the renderer on Fixed
 * HUD draws of a fill view ('M' elsewhere). */
int pw64_hud_vanchor = 'M';

/* Must match pw64-gfx `interp::VANCHOR_TAG` ("PWV" + 'T'/'M'/'B'). */
#define PW64_VANCHOR_TAG 0x50575600u

void pw64_hud_emit_anchor(int anchor) {
    if (pw64_widescreen_aspect <= 0.0f && pw64_level_players <= 1) {
        return;
    }
    gGfxDisplayListHead->words.w0 = (u32)G_NOOP << 24;
    gGfxDisplayListHead->words.w1 = PW64_ANCHOR_TAG | (u32)(anchor & 0xFF);
    gGfxDisplayListHead++;
}

/* The vertical anchor matters on a fill view (with or without
 * widescreen: 4:3 + fill shifts HUD rows too) and in split-screen views
 * (both anchors: pw64_hud_split_tag). */
void pw64_hud_emit_vanchor(int anchor) {
    if (pw64_widescreen_aspect <= 0.0f && !pw64_fill_screen && pw64_level_players <= 1) {
        return;
    }
    gGfxDisplayListHead->words.w0 = (u32)G_NOOP << 24;
    gGfxDisplayListHead->words.w1 = PW64_VANCHOR_TAG | (u32)(anchor & 0xFF);
    gGfxDisplayListHead++;
}

void pw64_hud_set_anchor(int anchor) {
    pw64_hud_anchor = anchor;
    /* Every HUD element without an explicit vertical anchor keeps its old
     * row: resetting to 'M' makes each set_anchor a complete anchor pair. */
    pw64_hud_vanchor = 'M';
    pw64_hud_emit_anchor(anchor);
    pw64_hud_emit_vanchor('M');
}

void pw64_hud_set_anchor2(int anchor, int vanchor) {
    pw64_hud_anchor = anchor;
    pw64_hud_vanchor = vanchor;
    pw64_hud_emit_anchor(anchor);
    pw64_hud_emit_vanchor(vanchor);
}

/* Anchor for a HUD element at N64 x `x` (bottom-up) `y`: the left gauge
 * column (x 27) and the right one (radar 215, sea level 235, altimeter 250)
 * go to the horizontal edges; y 160+ is the top row of the screen (timer,
 * radar), y < 100 the bottom (speed, fuel, throttle), else middle
 * (altimeter). */
void pw64_hud_anchor_xy(int x, int y) {
    pw64_hud_set_anchor2(x < 80 ? 'L' : x >= 200 ? 'R' : 'C',
                         y >= 160 ? 'T' : y < 100 ? 'B' : 'M');
}

/*
 * Flight-HUD on/off bracket. hud.c.patch calls pw64_hud_tag(1) at the top
 * of hudMainRender's non-disabled branch and pw64_hud_tag(0) at its end,
 * so the renderer knows which draws are the flight HUD (the only ones its
 * OLED care options drift + dim). Not gated on the widescreen/fill
 * settings: the OLED care setting is live in the renderer, which ignores
 * the tag unless it is on.
 */

/* Must match pw64-gfx `interp::HUD_TAG_ON/OFF` (G_NOOP w1). */
#define PW64_HUD_TAG_ON 0x50574831u  /* "PWH1" */
#define PW64_HUD_TAG_OFF 0x50574830u /* "PWH0" */

void pw64_hud_tag(int on) {
    gGfxDisplayListHead->words.w0 = (u32)G_NOOP << 24;
    gGfxDisplayListHead->words.w1 = on ? PW64_HUD_TAG_ON : PW64_HUD_TAG_OFF;
    gGfxDisplayListHead++;
}

/*
 * Movable HUD elements (renderer.md "Movable HUD"). hud.c.patch calls
 * pw64_hud_element(id) where each gauge starts; the renderer moves the
 * following flight-HUD draws by the player's offset for that element.
 * Text is queued, so font.c.patch stores the element per message like the
 * anchors and re-emits it (pw64_hud_emit_element) when drawing. Not gated
 * on any setting: offsets are live in the renderer, which ignores the tag
 * while they are zero.
 */

/* Must match pw64-gfx `interp::HUD_ELEM_TAG` ("PWE" + element id). */
#define PW64_HUD_ELEM_TAG 0x50574500u

/* Element for the following draws and text printed now (0 = none). */
int pw64_hud_elem = 0;

void pw64_hud_emit_element(int id) {
    gGfxDisplayListHead->words.w0 = (u32)G_NOOP << 24;
    gGfxDisplayListHead->words.w1 = PW64_HUD_ELEM_TAG | (u32)(id & 0xFF);
    gGfxDisplayListHead++;
}

void pw64_hud_element(int id) {
    if (id == pw64_hud_elem) {
        return;
    }
    pw64_hud_elem = id;
    pw64_hud_emit_element(id);
}

/*
 * Split-screen HUD (splitscreen.md "HUD per view"). Each player's HUD is
 * drawn in their channel's callback, still for the full 320x240 screen;
 * hudMainRender brackets it with this tag carrying the view (the current
 * player's channel viewport, top-down rows) and the renderer lays the
 * HUD's subscreen frame out inside it (pw64-gfx `split_placement`: one
 * uniform scale, each element kept at its L/C/R + T/M/B anchor, which
 * pw64_hud_emit_anchor/vanchor also emit in split-screen). One tag covers
 * every draw path (sprites, uvVtx gauges, queued text); the HUD scissor
 * stays full screen in the display list. Only with 2+ players in the test.
 */

void pw64_hud_split_tag(int on) {
    if (pw64_level_players <= 1) {
        return;
    }
    if (!on) {
        pw64_split_off_tag();
        return;
    }
    pw64_split_rect_tag(&D_80261730[pw64_player_cur]);
}
