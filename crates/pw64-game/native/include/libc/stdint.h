/* Shadows decomp include/libc/stdint.h: intptr_t/uintptr_t must be host
 * pointer width (the decomp's are s32/u32), so pointer<->integer casts
 * written with them stay lossless. */
#ifndef LIBC_STDINT_H
#define LIBC_STDINT_H

#include "PR/ultratypes.h"

typedef __INTPTR_TYPE__ intptr_t;
typedef __UINTPTR_TYPE__ uintptr_t;

#define INT8_MIN    (-0x80)
#define INT16_MIN   (-0x8000)
#define INT32_MIN   (-0x80000000)
#define INT64_MIN   (-0x8000000000000000)

#define INT8_MAX    0x7F
#define INT16_MAX   0x7FFF
#define INT32_MAX   0x7FFFFFFF
#define INT64_MAX   0x7FFFFFFFFFFFFFFF

#define UINT8_MAX   0xFF
#define UINT16_MAX  0xFFFF
#define UINT32_MAX  0xFFFFFFFF
#define UINT64_MAX  0xFFFFFFFFFFFFFFFF

#define INTPTR_MIN  INT64_MIN
#define INTPTR_MAX  INT64_MAX
#define UINTPTR_MAX UINT64_MAX

#endif
