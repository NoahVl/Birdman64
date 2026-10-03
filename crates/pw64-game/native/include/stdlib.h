/* Shadows the IDO stdlib.h (32-bit size_t). Only what the decomp uses;
 * resolved against the host CRT where `long` is 32-bit (Windows/LLP64). */
#ifndef _STDLIB_H
#define _STDLIB_H

typedef struct DIV_T { int quot; int rem; } div_t;

#ifndef NULL
#define NULL 0
#endif

int abs(int);
div_t div(int, int);

#if __SIZEOF_LONG__ == 4
typedef struct LDIV_T { long quot; long rem; } ldiv_t;
long labs(long);
ldiv_t ldiv(long, long);
#else
/* LP64 (Linux): the host ldiv is 64-bit; the decomp's (libultra) is 32-bit
 * and its callers (utils.c RANLUX) store the results in s32. */
typedef struct LDIV_T { int quot; int rem; } ldiv_t;
static inline ldiv_t pw64_ldiv(int n, int d) {
    ldiv_t r;
    r.quot = n / d;
    r.rem = n % d;
    return r;
}
#define ldiv pw64_ldiv
#endif

#endif
