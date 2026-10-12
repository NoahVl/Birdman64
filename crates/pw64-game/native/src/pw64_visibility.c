/* Optional geometric LOD and whole-island visibility. Texture mipmapping
 * remains enabled; it prevents shimmer and is separate from model LOD. */
#include <uv_graphics.h>
#include <uv_math.h>
#include <math_macros.h>
#include <macros.h>

float pw64_visibility_far(const void* chan, float original_far) {
    const UnkStruct_80204D94* c = chan;
    const ParsedUVTR* t;
    float dx, dy, dz, far;
    if (!pw64_infinite_view_distance || !(c->unk0 & 2) ||
        gGfxUnkPtrs == NULL || c->unk4 >= LEVEL_TERRA_COUNT) {
        return original_far;
    }
    t = gGfxUnkPtrs->terras[c->unk4];
    if (t == NULL) {
        return original_far;
    }
    dx = MAX(FABS(t->unk0.unk0 - c->unk1D0), FABS(t->unk0.unkC - c->unk1D0));
    dy = MAX(FABS(t->unk0.unk4 - c->unk1D4), FABS(t->unk0.unk10 - c->unk1D4));
    dz = MAX(FABS(t->unk0.unk8 - c->unk1D8), FABS(t->unk0.unk14 - c->unk1D8));
    /* Sky already uses a 27000-unit projection. Cover the terrain bounds
     * plus a margin for models, using finite arithmetic in the N64 matrix. */
    far = uvSqrtF(dx * dx + dy * dy + dz * dz) + t->unk24 + 1000.0f;
    return MAX(original_far, MAX(27000.0f, far));
}

extern Gfx gGfxDisplayListBase[2][PW64_GFX_DL_CAPACITY];

/* Every packet-producing GBI macro calls this before writing. Only the
 * active frame buffer is bounded here; asset-owned display lists remain
 * writable by their existing loaders. A full frame fails before corruption. */
void* pw64_gfx_packet(void* packet) {
    if (packet == &gGfxDisplayListBase[gGfxFbIndex][PW64_GFX_DL_CAPACITY]) {
        pw64_fatal("native display list capacity exceeded", PW64_GFX_DL_CAPACITY);
    }
    return packet;
}
