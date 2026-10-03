/*
 * Compile-time checks for host struct layouts that Rust or the swap layouts
 * rely on (native-build.md §3/§5). No code.
 */
#include <ultra64.h>
#include <PR/sched.h>
#include <libc/stddef.h>
#include <uv_filesystem.h>

/* pw64-platform headless.rs `osSpTaskStartGo` reads these at fixed offsets
 * from the `OSTask*` (= &OSScTask.list). */
_Static_assert(offsetof(OSTask_t, data_ptr) == 88, "OSTask.data_ptr");
_Static_assert(offsetof(OSTask_t, data_size) == 96, "OSTask.data_size");
_Static_assert(offsetof(OSScTask, list) - offsetof(OSScTask, flags) == 12, "OSScTask.flags");
_Static_assert(offsetof(OSScTask, list) - offsetof(OSScTask, framebuffer) == 8, "OSScTask.framebuffer");

/* N64 sizes that raw ROM copies and dlist strides depend on. */
_Static_assert(sizeof(Gfx) == 8, "Gfx (MSVC bitfield layout, gbi.h.patch)");
_Static_assert(sizeof(Vtx) == 16, "Vtx");
_Static_assert(sizeof(Mtx) == 64, "Mtx");
_Static_assert(sizeof(UnkCommStruct) == 0x18, "UnkCommStruct");
_Static_assert(sizeof(UnkPartStruct_Unk8) == 0x14, "UnkPartStruct_Unk8");

/* snap.h photo-album record: IDO 0x18, MSVC (new bitfield unit after the
 * u8s) 0x1C, SysV/GCC 0x18 (packs across types like IDO, but LSB-first). The
 * EEPROM bitstream never sees the host layout: snap.c.patch serialises the
 * IDO byte image field by field (pw64_photoToIdo/FromIdo). If this fires, the
 * layout changed; re-check that patch rather than the number. */
#include "app/snap.h"
#ifdef _MSC_VER
_Static_assert(sizeof(Unk8033F050) == 0x1C, "Unk8033F050 host layout (snap.c.patch)");
#else
_Static_assert(sizeof(Unk8033F050) == 0x18, "Unk8033F050 host layout (snap.c.patch)");
#endif
