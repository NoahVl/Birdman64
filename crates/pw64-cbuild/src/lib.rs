//! Compiler flag sets and patch plumbing shared by the native C builds of the
//! decomp sources:
//!
//! - `pw64-game`'s build.rs (dev builds: static archive, zig; legacy clang-cl / clang)
//! - the first-run builder (release: game module with zig, per decomp file)
//!
//! Flags live here so both produce the same command lines by construction
//! (docs/notes/first-run-build.md, "Flags (must equal the static build)").
//!
//! Patches travel as context-free ops (`ops`) and the decomp sources are
//! hash-pinned by the committed manifest (`manifest`), see the task list in
//! docs/notes/first-run-build.md.

pub mod build;
#[cfg(feature = "fetch")]
pub mod fetch;
pub mod kit;
pub mod manifest;
pub mod ops;

/// The embedded build kit (T8): `native/**`, `dylib/*` and the context-free
/// ops, packed by pw64-cbuild's own `build.rs` into one deterministic byte
/// string (`kit` module docs). Read by [`build::build_module`] and hashed
/// into the ABI string (`kit::abi`); the release exe ships it inside itself.
pub static KIT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kit.bin"));

/// The one C compiler of the project (T13): the dev/test build (pw64-game
/// build.rs), CI and the players' first-run builder all use this zig
/// (`zig clang`, clang 21 inside). build.rs refuses any other version.
pub const ZIG_VERSION: &str = "0.16.0";

/// One official zig release archive: download URL, its sha256, and the path
/// of the zig binary inside it (the only file we need: no `lib/`).
pub struct ZigDist {
    pub url: &'static str,
    pub sha256: &'static str,
    pub exe_in_archive: &'static str,
}

/// [`ZIG_VERSION`] for x86_64 Windows (zip). Same URL + hash as release.yml
/// and ci.yml.
pub const ZIG_WINDOWS: ZigDist = ZigDist {
    url: "https://ziglang.org/download/0.16.0/zig-x86_64-windows-0.16.0.zip",
    sha256: "68659eb5f1e4eb1437a722f1dd889c5a322c9954607f5edcf337bc3684a75a7e",
    exe_in_archive: "zig-x86_64-windows-0.16.0/zig.exe",
};

/// [`ZIG_VERSION`] for x86_64 Linux (tar.xz).
pub const ZIG_LINUX: ZigDist = ZigDist {
    url: "https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz",
    sha256: "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00",
    exe_in_archive: "zig-x86_64-linux-0.16.0/zig",
};

/// One stage of the first-run build (docs/notes/first-run-build.md, "Steps +
/// UX"): the launcher (T9) draws the progress window from these. Download and
/// VerifyExtract are reported by [`fetch`], the rest by [`build`]; `Load`
/// (100%) belongs to the module loader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    FindZig,
    Download,
    VerifyExtract,
    ApplyPatches,
    Compile,
    Link,
    Load,
}

/// Progress callback argument: the current step plus the overall fraction of
/// the whole first-run build (0.0 = start, 1.0 = ready to load), matching the
/// percents in the notes (FindZig 0%, download 5-15%, verify+extract 20%,
/// patches 25%, compile 25-95% per file, link 97%, load 100%).
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub step: Step,
    pub fraction: f32,
}

/// Decomp source directories compiled natively (relative to `decomp/`).
/// libultra's os/io/libc are replaced by Rust (`pw64-platform`); the audio and
/// sprite libraries are portable C.
pub const SRC_DIRS: &[&str] = &[
    "src/kernel",
    "src/app",
    "src/libultra/audio",
    "src/libultra/sp",
];

/// Decomp include dirs, mirroring the Makefile's INCLUDE_CFLAGS (minus the IDO
/// libc; `compiler/` is added for `sgidefs.h`'s `#include "gcc/sgidefs.h"`).
pub const DECOMP_INCLUDES: &[&str] = &[
    ".",
    "src",
    "include",
    "include/kernel",
    "include/libultra",
    "include/libultra/PR",
    "include/libultra/compiler",
];

pub const DEFINES: &[&str] = &[
    "_LANGUAGE_C",
    "VERSION_US",
    "BUILD_VERSION=VERSION_D",
    "_FINALROM",
    "NDEBUG",
    "TARGET_N64",
    "NON_MATCHING",
    "AVOID_UB",
    // Makes STATIC_DATA/STATIC_FUNC non-static so Rust can reach them.
    "RECOMP_BUILD",
];

/// Code-generation flags, clang-cl spelling (Windows target). Every entry
/// has a GNU-spelled twin in [`GNU_FLAGS`] (Linux target): keep them in sync.
pub const MSVC_FLAGS: &[&str] = &[
    "/clang:-nostdinc",
    "/clang:-std=gnu11",
    // IDO chars are unsigned.
    "-J",
    "/clang:-fwrapv",
    "/clang:-fno-strict-aliasing",
    // The VR4300 has no FMA; keep float results bit-comparable.
    "/clang:-ffp-contract=off",
    // Decomp reads uninitialised locals (`@bug` notes, e.g. `sp5B` in
    // hangGliderMovementFrame): clang treats that as UB and dropped the rest
    // of the function (fell into int3). Zero-init makes them defined.
    "/clang:-ftrivial-auto-var-init=zero",
    // clang-cl doesn't define __GNUC__, but the decomp headers key
    // __attribute__/ALIGNED/NORETURN/va_list on it.
    "/clang:-fgnuc-version=4.2.1",
];

/// [`MSVC_FLAGS`] for clang in GNU mode (x86_64-unknown-linux-gnu target).
pub const GNU_FLAGS: &[&str] = &[
    "-nostdinc",
    "-std=gnu11",
    "-funsigned-char",
    "-fwrapv",
    "-fno-strict-aliasing",
    "-ffp-contract=off",
    "-ftrivial-auto-var-init=zero",
    // GNU-mode clang already defines __GNUC__ 4.2.1; pinned for parity.
    "-fgnuc-version=4.2.1",
    // Position-independent *code*, although the exe is linked non-PIE at
    // 0xC0000000: non-PIC x86-64 code (small code model) addresses globals
    // with sign-extended 32-bit immediates (R_X86_64_32S), which can't reach
    // above 2 GB. RIP-relative code works at any base. (cc passes -fPIC;
    // replaced, since -fPIE also binds local symbols directly.)
    "-fPIE",
];

/// Diagnostics, same spelling for clang-cl and clang (both targets).
pub const WARN_FLAGS: &[&str] = &[
    "-Wno-multichar",
    // os_libc.h declares bcopy/bzero/bcmp with IDO signatures.
    "-Wno-incompatible-library-redeclaration",
    // Pointer-width diagnostics are errors: the pointer patch set brought them
    // to 0 (native-build.md §2), so any new one is a regression. Fix it with
    // PW64_U32/PW64_PTR (pw64_native.h) in a patch, never with a bare cast.
    "-Werror=int-conversion",
    "-Werror=pointer-to-int-cast",
    "-Werror=int-to-pointer-cast",
    "-Werror=void-pointer-to-int-cast",
    "-Werror=int-to-void-pointer-cast",
    "-Werror=shorten-64-to-32",
    "-Wno-error=incompatible-function-pointer-types",
    "-Wno-error=incompatible-pointer-types",
    "-Wno-error=implicit-function-declaration",
    "-Wno-error=implicit-int",
    "-Wno-error=return-type",
];

/// Host target of the C build (build scripts run on the host, so the target
/// is decided by what the crate compiles for, not `cfg!(windows)`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    /// zig clang, x86_64-pc-windows-msvc (LLP64, MSVC bitfields, PE).
    Windows,
    /// zig clang, x86_64-unknown-linux-gnu (LP64, SysV bitfields, ELF).
    Linux,
}

/// `dylib/spike_build.sh`'s `TGT` (per host).
pub(crate) const ZIG_TGT_WINDOWS: &[&str] = &[
    "--target=x86_64-pc-windows-msvc",
    "-D_MT",
    "-D_DLL",
    // = clang-cl -MD -O2 (cc1 -O3, /GS strong, /Gy, /Oy), checked with -###
    // against the static build's c-flags.txt (spike_build.sh).
    "-fstack-protector-strong",
    "-ffunction-sections",
    "-fomit-frame-pointer",
];

const ZIG_TGT_LINUX: &[&str] = &["--target=x86_64-unknown-linux-gnu", "-fPIE"];

/// `dylib/spike_build.sh`'s `COMMON` (GNU spelling, both hosts; the twin test
/// below pins it against [`MSVC_FLAGS`]/[`GNU_FLAGS`]).
const ZIG_COMMON: &[&str] = &[
    "-nostdinc",
    "-std=gnu11",
    "-funsigned-char",
    "-fwrapv",
    "-fno-strict-aliasing",
    "-ffp-contract=off",
    "-ftrivial-auto-var-init=zero",
    "-fgnuc-version=4.2.1",
    "-O3",
];

/// zig flag sets for the first-run game module: exactly `dylib/spike_build.sh`'s
/// `TGT` + `COMMON` (+ [`WARN_FLAGS`], [`DEFINES`], include template), in that
/// order. zig clang is a plain clang driver, so the GNU spelling applies on
/// both hosts (`zig cc`'s defaults and VS/SDK probing are bypassed on purpose,
/// see the notes).
///
/// The include template mirrors the static build (spike_build.sh `INCS`):
/// `-I<game>/native/include` plus one `-I` per [`DECOMP_INCLUDES`] entry
/// rooted at the (patched) decomp tree; `<native>`/`<tree>` are the
/// builder's placeholders. `-include native/pw64_native.h` is the force-
/// include from the static build's `-FI`.
pub fn zig_flags(os: Os) -> Vec<String> {
    let tgt = match os {
        Os::Windows => ZIG_TGT_WINDOWS,
        Os::Linux => ZIG_TGT_LINUX,
    };
    let mut flags: Vec<String> = tgt
        .iter()
        .chain(ZIG_COMMON.iter())
        .chain(WARN_FLAGS.iter())
        .map(|s| s.to_string())
        .collect();
    flags.push("-include".into());
    flags.push("<native>/pw64_native.h".into());
    flags.extend(DEFINES.iter().map(|d| format!("-D{d}")));
    flags.push("-I<native>/include".into());
    flags.extend(DECOMP_INCLUDES.iter().map(|i| format!("-I<tree>/{i}")));
    flags
}

/// The `TGT` + `COMMON` part of [`zig_flags`] (no WARN flags, defines or
/// includes): the shim (`pw64_dll_shim.c`, our file, no decomp headers) is
/// compiled with these plus `-DPW64_DLL_ABI=<abi>` and `-I<dylib>` only,
/// exactly like `spike_build.sh`.
pub fn zig_tgt_common(os: Os) -> Vec<String> {
    let tgt = match os {
        Os::Windows => ZIG_TGT_WINDOWS,
        Os::Linux => ZIG_TGT_LINUX,
    };
    tgt.iter()
        .chain(ZIG_COMMON.iter())
        .map(|s| s.to_string())
        .collect()
}

/// Strict applier of unified diffs (moved verbatim from pw64-game's build.rs):
/// no fuzz. Compares lines ignoring CR, and keeps the original file's line
/// endings (the decomp checkout may be CRLF).
pub fn apply_unified_diff(original: &str, diff: &str) -> Result<String, String> {
    let eol = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let src: Vec<&str> = original.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut out: Vec<String> = Vec::new();
    let mut pos = 0usize; // next unconsumed line of `src`
    let mut lines = diff.lines().map(|l| l.trim_end_matches('\r')).peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("@@ -") else {
            continue;
        };
        let old_start: usize = rest
            .split([',', ' '])
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| format!("bad hunk header: {line}"))?;
        let start = old_start.saturating_sub(1);
        if start < pos {
            return Err(format!("overlapping hunk: {line}"));
        }
        out.extend(src[pos..start].iter().map(|s| s.to_string()));
        pos = start;
        while let Some(&h) = lines.peek() {
            if h.starts_with("@@") || h.starts_with("--- ") || h.starts_with("diff ") {
                break;
            }
            lines.next();
            if h.starts_with('\\') {
                continue; // "\ No newline at end of file"
            }
            let (tag, text) = h.split_at(h.len().min(1));
            match tag {
                " " | "" => {
                    if src.get(pos) != Some(&text) {
                        return Err(format!("context mismatch at line {}: {text:?}", pos + 1));
                    }
                    out.push(text.to_string());
                    pos += 1;
                }
                "-" => {
                    if src.get(pos) != Some(&text) {
                        return Err(format!(
                            "removed line mismatch at line {}: {text:?}",
                            pos + 1
                        ));
                    }
                    pos += 1;
                }
                "+" => out.push(text.to_string()),
                _ => return Err(format!("bad diff line: {h:?}")),
            }
        }
    }
    out.extend(src[pos..].iter().map(|s| s.to_string()));
    let mut s = out.join(eol);
    s.push_str(eol);
    Ok(s)
}

/// Whether `line` (already column-0) defines `name`: the name is a whole word
/// followed only by whitespace before `(`.
pub fn defines_at_column0(line: &str, name: &str) -> bool {
    // A column-0 prototype in some other .c file (`void f(int);`) is a
    // declaration, not a second definition.
    if line.starts_with(char::is_whitespace) || line.trim_end().ends_with(';') {
        return false;
    }
    let bytes = line.as_bytes();
    let nb = name.as_bytes();
    if bytes.len() < nb.len() {
        return false;
    }
    let word = |p: usize| p == 0 || !(bytes[p - 1].is_ascii_alphanumeric() || bytes[p - 1] == b'_');
    for p in 0..=(bytes.len() - nb.len()) {
        if &bytes[p..p + nb.len()] == nb && word(p) {
            let mut q = p + nb.len();
            while q < bytes.len() && bytes[q].is_ascii_whitespace() {
                q += 1;
            }
            if q < bytes.len() && bytes[q] == b'(' {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every MSVC spelling has its GNU twin on this table (and vice versa):
    /// zig clang is a plain clang driver on both hosts, so the zig build
    /// always uses the GNU spelling; the static build uses MSVC_FLAGS on
    /// Windows and GNU_FLAGS on Linux. Keep the code identical on each
    /// target by construction.
    const TWINS: &[(&str, &str)] = &[
        ("/clang:-nostdinc", "-nostdinc"),
        ("/clang:-std=gnu11", "-std=gnu11"),
        // IDO chars are unsigned: clang-cl's -J is GNU clang's
        // -funsigned-char (the spike passes the GNU spelling on both hosts).
        ("-J", "-funsigned-char"),
        ("/clang:-fwrapv", "-fwrapv"),
        ("/clang:-fno-strict-aliasing", "-fno-strict-aliasing"),
        ("/clang:-ffp-contract=off", "-ffp-contract=off"),
        (
            "/clang:-ftrivial-auto-var-init=zero",
            "-ftrivial-auto-var-init=zero",
        ),
        ("/clang:-fgnuc-version=4.2.1", "-fgnuc-version=4.2.1"),
    ];

    #[test]
    fn zig_flag_twin_table() {
        // Every MSVC_FLAGS entry is on the twin table exactly once ...
        assert_eq!(MSVC_FLAGS.len(), TWINS.len(), "twin table incomplete");
        for f in MSVC_FLAGS {
            assert!(
                TWINS.iter().any(|(m, _)| *m == *f),
                "{f}: no GNU twin in the table"
            );
        }
        // ... every GNU_FLAGS entry except -fPIE (PE objects need no PIC on
        // Windows; the static Linux build gets it from GNU_FLAGS, the zig
        // Linux TGT re-adds it) ...
        for f in GNU_FLAGS {
            assert!(
                TWINS.iter().any(|(_, g)| g == f) || *f == "-fPIE",
                "{f}: no MSVC twin in the table"
            );
        }
        // ... and each pair appears exactly once on its own side.
        for (m, g) in TWINS {
            assert_eq!(
                MSVC_FLAGS.iter().filter(|f| *f == m).count(),
                1,
                "{m}: not exactly once"
            );
            assert_eq!(
                GNU_FLAGS.iter().filter(|f| *f == g).count(),
                1,
                "{g}: not exactly once"
            );
        }
        // The remaining MSVC-only twins (/Gy, /Oy) come out of clang-cl's cc
        // baseline (-O2), not MSVC_FLAGS; the zig TGT spells them out.
        for f in ["-ffunction-sections", "-fomit-frame-pointer", "-O3"] {
            assert!(
                zig_flags(Os::Windows).iter().any(|g| g == f),
                "{f}: missing from zig_flags(Windows)"
            );
        }
        // Spike parity: the flag bodies equal spike_build.sh verbatim.
        let win = zig_flags(Os::Windows);
        let lin = zig_flags(Os::Linux);
        for f in ZIG_COMMON {
            assert!(lin.iter().any(|g| g == f), "{f}: missing from zig Linux");
            assert!(win.iter().any(|g| g == f), "{f}: missing from zig Windows");
        }
        assert!(lin.iter().any(|g| g == "--target=x86_64-unknown-linux-gnu"));
        assert!(lin.iter().any(|g| g == "-fPIE"));
        assert!(win.iter().any(|g| g == "--target=x86_64-pc-windows-msvc"));
        assert!(win.iter().any(|g| g == "-D_MT") && win.iter().any(|g| g == "-D_DLL"));
        assert!(win.iter().any(|g| g == "-fstack-protector-strong"));
    }

    #[test]
    fn zig_flags_order_and_includes() {
        // TGT, COMMON, WARN, -include, DEFINES, includes: spike_build.sh's
        // FLAGS order. Both hosts agree except the TGT part.
        for (os, tgt_len) in [
            (Os::Windows, ZIG_TGT_WINDOWS.len()),
            (Os::Linux, ZIG_TGT_LINUX.len()),
        ] {
            let flags = zig_flags(os);
            let mut i = 0;
            for t in match os {
                Os::Windows => ZIG_TGT_WINDOWS,
                Os::Linux => ZIG_TGT_LINUX,
            } {
                assert_eq!(flags[i], *t);
                i += 1;
            }
            for c in ZIG_COMMON {
                assert_eq!(flags[i], *c);
                i += 1;
            }
            for w in WARN_FLAGS {
                assert_eq!(flags[i], *w);
                i += 1;
            }
            assert_eq!(flags[i], "-include");
            assert_eq!(flags[i + 1], "<native>/pw64_native.h");
            i += 2;
            for d in DEFINES {
                assert_eq!(flags[i], format!("-D{d}"));
                i += 1;
            }
            assert_eq!(flags[i], "-I<native>/include");
            i += 1;
            for inc in DECOMP_INCLUDES {
                assert_eq!(flags[i], format!("-I<tree>/{inc}"));
                i += 1;
            }
            assert_eq!(i, flags.len(), "zig_flags: unexpected trailing flags");
            assert!(tgt_len > 0);
        }
    }
}
