/* Regression probe against the actual patched C selectors/channel code.
 * Matrix upload and plane refresh are stubbed; game flights cover those. */
#include <uv_chan.h>
#include <uv_dobj.h>
#include <uv_sobj.h>
#include <uv_math.h>
int printf(const char*, ...);
void* memset(void*, int, size_t);
float sqrtf(float);
_Noreturn void exit(int);

int pw64_infinite_lod;
int pw64_infinite_view_distance;
Gfx gGfxDisplayListBase[2][PW64_GFX_DL_CAPACITY];
u16 gGfxFbIndex = 0;
_Noreturn void pw64_fatal(const char* message, unsigned int value) {
    printf("EXPECTED CAPACITY FAILURE: %s (%u)\n", message, value);
    exit(86);
}
static uvLevelData level;
uvLevelData* gGfxUnkPtrs = &level;

void func_802061A0(UnkStruct_80204D94* c) { (void)c; }
void uvMat4CopyF2L(Mtx* dst, Mtx4F* src) { (void)src; memset(dst, 0, sizeof(*dst)); }
void uvMat4SetFrustrum(Mtx4F* dst, float l, float r, float t, float b, float n, float f) {
    (void)l; (void)r; (void)t; (void)b;
    memset(dst, 0, sizeof(*dst));
    dst->m[2][2] = -(f + n) / (f - n);
}
float uvSqrtF(float x) { return sqrtf(x); }

#define CHECK(x) do { if (!(x)) { printf("FAIL line %d: %s\n", __LINE__, #x); return 1; } } while (0)

int main(int argc, char** argv) {
    float radii[] = {100.0f, 500.0f, 1000.0f};
    float distances[] = {0.0f, 100.0f, 101.0f, 500.0f, 501.0f, 999.0f, 1000.0f, 30000.0f};
    unsigned char original[] = {0, 0, 1, 1, 2, 2, 255, 255};
    ParsedUVMD model = {0};
    ParsedUVTR terrain = {0};
    UnkStruct_80204D94* c = &D_80261730[0];
    int i;
    CHECK(argc == 3);
    if (argv[1][0] == '2') {
        /* Two-packet macro starting in the final slot must stop before
         * writing the following frame's display list. */
        Gfx* p = &gGfxDisplayListBase[0][PW64_GFX_DL_CAPACITY - 1];
        gDPTextureRectangle(p, 0, 0, 4, 4, 0, 0, 0, 32, 32);
        return 0;
    }
    if (argv[1][0] == '3') {
        gSPNoOp(&gGfxDisplayListBase[0][PW64_GFX_DL_CAPACITY]);
        return 0;
    }
    pw64_infinite_lod = argv[1][0] == '1';
    pw64_infinite_view_distance = argv[2][0] == '1';
    model.lodRadius = radii;
    model.lodCount = 3;
    for (i = 0; i < 8; i++) {
        unsigned char expected = pw64_infinite_lod ? 0 : original[i];
        if (!pw64_infinite_lod && pw64_infinite_view_distance && expected == 255) expected = 2;
        CHECK(uvSobjGetLODIndex(&model, distances[i]) == expected);
        CHECK(uvDobjGetLODIndex(&model, distances[i]) == expected);
    }
    model.lodCount = 0;
    CHECK(uvSobjGetLODIndex(&model, 0) == 255);
    CHECK(uvDobjGetLODIndex(&model, 0) == 255);
    /* A terrain channel must reach the farthest map corner, even when the
     * camera is outside the island. Menu model channels keep their clip. */
    terrain.unk0.unkC = 50000.0f;
    terrain.unk0.unk10 = 50000.0f;
    terrain.unk0.unk14 = 2000.0f;
    level.terras[0] = &terrain;
    c->unk0 = 2;
    c->unk1D0 = -1000.0f;
    uvChan_80204C94(0, -20, 20, 15, -15, 40, 2000);
    CHECK(c->unk1F8 == 40.0f);
    CHECK(__builtin_isfinite(c->unk10.m[2][2]));
    CHECK(pw64_infinite_view_distance ? c->unk1FC > 71000.0f : c->unk1FC == 2000.0f);
    c->unk0 = 0;
    uvChan_80204C94(0, -20, 20, 15, -15, 40, 2000);
    CHECK(c->unk1FC == 2000.0f);
    printf("PASS LOD=%d view=%d\n", pw64_infinite_lod, pw64_infinite_view_distance);
    return 0;
}
