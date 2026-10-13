/* Optional model detail and whole-island view distance. Model LOD only:
 * texture mipmapping stays, it prevents shimmer and is separate. */
#include <uv_graphics.h>
#include <uv_math.h>
#include <math_macros.h>
#include <macros.h>

/* Set before boot by pw64_game::set_visibility. LOD selection reads both
 * (dobj.c.patch, sobj.c.patch), drawing the view distance (chan.c.patch). */
int pw64_infinite_lod = 0;
int pw64_infinite_view_distance = 0;

/* Far plane for drawing channel `chan` (uvChan_80204FE4): with infinite
 * view distance, far enough to reach every corner of its terrain from the
 * camera, plus a margin for models standing on the edge; else `far`. */
float pw64_visibility_far(const void* chan, float far) {
    const UnkStruct_80204D94* c = chan;
    const ParsedUVTR* t;
    float dx, dy, dz;

    if (!pw64_infinite_view_distance || !(c->unk0 & 2) || c->unk4 >= LEVEL_TERRA_COUNT) {
        return far;
    }
    t = gGfxUnkPtrs->terras[c->unk4];
    if (t == NULL) {
        return far;
    }
    dx = MAX(FABS(t->unk0.unk0 - c->unk1D0), FABS(t->unk0.unkC - c->unk1D0));
    dy = MAX(FABS(t->unk0.unk4 - c->unk1D4), FABS(t->unk0.unk10 - c->unk1D4));
    dz = MAX(FABS(t->unk0.unk8 - c->unk1D8), FABS(t->unk0.unk14 - c->unk1D8));
    return MAX(far, uvSqrtF(SQ(dx) + SQ(dy) + SQ(dz)) + t->unk24 + 1000.0f);
}
