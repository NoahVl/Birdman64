/* Shadows decomp include/libc/stddef.h: host-width ptrdiff_t. */
#ifndef LIBC_STDDEF_H
#define LIBC_STDDEF_H

#include "PR/ultratypes.h"

typedef __PTRDIFF_TYPE__ ptrdiff_t;

#define offsetof(structure, member) __builtin_offsetof(structure, member)

#endif
