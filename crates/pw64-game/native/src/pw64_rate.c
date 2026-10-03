/* Frame-rate independence state (native/include/pw64_rate.h). */
#include <pw64_rate.h>

/* N64 frames per current frame; func_80313D74 (code_9A960.c) sets it. */
float pw64_rate_scale = 1.0f;
