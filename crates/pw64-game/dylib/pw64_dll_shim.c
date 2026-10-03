/*
 * Glue for the first-run build (docs/notes/first-run-build.md): the decomp C
 * is compiled on the player's machine into a DLL (Windows) / .so (Linux)
 * loaded at a fixed address below 4 GB by the shipped exe (pw64-game
 * feature `dylib`). This file is ours and is compiled into that module.
 *
 * - Imports: the C calls ~50 Rust functions (libultra replacements, pw64_*).
 *   Instead of linking against the exe (import lib naming "birdman64.exe",
 *   -rdynamic), every such name is defined here as a one-instruction jump
 *   through a slot; the exe fills the slots via `pw64_dll_bind` right after
 *   loading. The module then has no imports from the exe: renaming the exe
 *   is harmless and the same code works for PE and ELF. A tail jump builds
 *   no frame, so unwinding (Rust panics through C frames) is unaffected.
 * - CRT (T1): memcpy/memset/strlen/ldiv/powf/sqrtf are thunks too
 *   (`C(name)` in pw64_dll_imports.h), bound to pw64-platform's
 *   `pw64_crt_*` (src/crt.rs). The module imports nothing at all: no
 *   ucrtbase/libc, the exe's CRT everywhere (same `powf` on every machine;
 *   Linux: no `--as-needed` risk of libm being dropped from the exe).
 * - Data the static build gets from pw64-platform (osTvType, osMemSize,
 *   osViModeTable, ucode symbols) is defined here instead: nothing on the
 *   Rust side reads it. Keep the values in sync with pw64-platform
 *   (os/misc.rs, stubs.rs).
 * - MSVC target only: /GS cookie and `_fltused`, which the MSVC CRT
 *   would otherwise provide (the module links no CRT).
 */

/* Built with -nostdinc: no headers. */
typedef __SIZE_TYPE__ size_t;
#define NULL ((void*)0)

#ifndef PW64_DLL_ABI
#error "build with -DPW64_DLL_ABI=\"...\" (the exe's expected ABI string)"
#endif

#if defined(_WIN32)
#define PW64_EXPORT __declspec(dllexport)
#else
#define PW64_EXPORT __attribute__((visibility("default")))
#endif

/* --- import slots + jump thunks --- */

#define X(name) void* pw64_imp_##name;
#define C(name) void* pw64_imp_##name;
#include "pw64_dll_imports.h"
#undef X
#undef C

#if defined(_WIN32)
#define PW64_THUNK(name)                                                     \
    __asm__(".text\n.globl " #name "\n.def " #name ";.scl 2;.type 32;.endef\n" \
            ".p2align 4\n" #name ":\n\tjmp *pw64_imp_" #name "(%rip)\n");
#else
#define PW64_THUNK(name)                                                     \
    __asm__(".text\n.globl " #name "\n.hidden " #name "\n.type " #name        \
            ",@function\n.p2align 4\n" #name ":\n\tjmp *pw64_imp_" #name      \
            "(%rip)\n.size " #name ",.-" #name "\n");
#endif
#define X(name) PW64_THUNK(name)
#define C(name) PW64_THUNK(name)
#include "pw64_dll_imports.h"
#undef X
#undef C

static const char* const pw64_import_names[] = {
#define X(name) #name,
#define C(name) #name,
#include "pw64_dll_imports.h"
#undef X
#undef C
};

static void** const pw64_import_slots[] = {
#define X(name) &pw64_imp_##name,
#define C(name) &pw64_imp_##name,
#include "pw64_dll_imports.h"
#undef X
#undef C
};

/* Fills every slot with `lookup(name)`. Returns 0, or 1 + the index of the
 * first name `lookup` doesn't know (the exe reports it). */
PW64_EXPORT int pw64_dll_bind(void* (*lookup)(const char* name)) {
    size_t i;
    for (i = 0; i < sizeof(pw64_import_names) / sizeof(pw64_import_names[0]); i++) {
        void* p = lookup(pw64_import_names[i]);
        if (p == NULL) {
            return (int)i + 1;
        }
        *pw64_import_slots[i] = p;
    }
    return 0;
}

/* The exe refuses a module built for another version (cache key check). */
PW64_EXPORT const char* pw64_dll_abi(void) {
    return PW64_DLL_ABI;
}

/* [start, end) of this module's mapped image: the exe asserts it is at the
 * fixed base with bit 31 set, and its HLE maps these addresses 1:1. */
#if defined(_WIN32)
extern const unsigned char __ImageBase[]; /* lld-link / link.exe */
PW64_EXPORT void pw64_dll_range(const void** start, const void** end) {
    const unsigned char* nt = __ImageBase + *(const unsigned int*)(__ImageBase + 0x3C);
    *start = __ImageBase;
    /* SizeOfImage: PE sig 4 + file header 20 + optional header 56. */
    *end = __ImageBase + *(const unsigned int*)(nt + 24 + 56);
}
#else
extern const unsigned char __executable_start[], _end[]; /* lld */
PW64_EXPORT void pw64_dll_range(const void** start, const void** end) {
    *start = __executable_start;
    *end = _end;
}
#endif

/* --- libultra data the static build takes from pw64-platform --- */

int osTvType = 1;                   /* OS_TV_NTSC; os/misc.rs */
unsigned int osMemSize = 0x800000;  /* 8 MB; os/misc.rs */
/* 56 modes x 0x60 (host OSViMode + headroom), zero; stubs.rs */
_Alignas(8) unsigned char osViModeTable[56 * 0x60];
/* RSP ucode symbols: only their addresses go into OSTask (stubs.rs). */
long long rspbootTextStart[1], rspbootTextEnd[1];
long long gspFast3DTextStart[1], gspFast3DDataStart[1];
long long gspF3DEX_fifoTextStart[1], gspF3DEX_fifoDataStart[1];
long long aspMainTextStart[1], aspMainDataStart[1];

/* --- MSVC CRT pieces the compiler references (no CRT is linked) --- */
#if defined(_MSC_VER)
/* clang emits a reference whenever floating point is used. */
int _fltused = 0x9875;

/* /GS (clang-cl's default, kept for parity with the static build): the
 * cookie stays at MSVC's default value (no DllMain to randomise it; it is
 * an overrun detector here, not a security boundary). */
unsigned long long __security_cookie = 0x00002B992DDFA232ULL;

void __security_check_cookie(unsigned long long cookie) {
    if (cookie != __security_cookie) {
        __fastfail(2); /* FAST_FAIL_STACK_COOKIE_CHECK_FAILURE, as the CRT */
    }
}
#endif
