/*
 * Frame-rate independence (docs/notes/framerate.md). The game is dt-driven
 * (D_8034F854), but some code steps a value once per call, i.e. once per N64
 * frame. These helpers rescale such steps to PW64_REF_DT, the N64 frame time
 * the constants were tuned at, so the game behaves the same at any host rate.
 * Branch-free; valid for the scale range func_80313D74 produces ([0.06, 3]).
 */
#ifndef PW64_RATE_H
#define PW64_RATE_H

/* N64 frame time the per-frame constants assume (calibrate later). */
#define PW64_REF_DT (1.0f / 30.0f)
/* func_80313D74 dt clamp (was [0.01, 0.1]; 0.002 = 500 Hz). */
#define PW64_DT_MIN 0.002f
#define PW64_DT_MAX 0.1f

/* N64 frames per current frame. Set once per frame by func_80313D74
 * (code_9A960.c): dt / PW64_REF_DT, or real_dt / PW64_REF_DT while a
 * uvGfxSetFrameTime sentinel slows game time (per-frame visuals then still
 * run at the N64 frame rate). Defined in native/src/pw64_rate.c. */
extern float pw64_rate_scale;

float powf(float, float);
float sqrtf(float);

/* Per-frame step `x += c` -> `x += c * pw64_scale()`. */
static inline float pw64_scale(void) {
    return pw64_rate_scale;
}

/* Same for an explicit frame time. */
static inline float pw64_scale_dt(float dt) {
    return dt * (1.0f / PW64_REF_DT);
}

/* Per-frame factor `x *= k` (k > 0) -> `x *= pw64_decay(k)`: same decay per
 * second as k per REF frame. */
static inline float pw64_decay(float k) {
    return powf(k, pw64_rate_scale);
}

/* For `counter++` per frame: adds this frame's REF frames to *acc and returns
 * the whole ones (0..3), keeping the fraction in *acc. *acc starts at 0. */
static inline int pw64_ticks(float* acc) {
    float a = *acc + pw64_rate_scale;
    int n = (int)a;
    *acc = a - (float)n;
    return n;
}

/* Random walk with a per-frame step: `x += (rand - 0.5) * r * dt` spreads as
 * sqrt(dt); multiply the step by pw64_walk() for the spread of REF frames. */
static inline float pw64_walk(void) {
    return sqrtf(1.0f / pw64_rate_scale);
}

/* Wall time of the last frame as uvGfxEnd measured it (graphics.c, before
 * any uvGfxSetFrameTime override), clamped like dt. For loops that don't
 * call func_80313D74 (menus, fades). */
extern float gGfxFrameTime[2];
extern unsigned short gGfxFbIndex;
static inline float pw64_real_dt(void) {
    return __builtin_fminf(__builtin_fmaxf(gGfxFrameTime[gGfxFbIndex ^ 1], PW64_DT_MIN), PW64_DT_MAX);
}

#endif
