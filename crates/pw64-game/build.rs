//! Compiles the decomp's C (kernel + app + libultra audio/sprite libs) for the
//! host with zig 0.16.0 (`zig clang`, the first-run builder's flags; legacy
//! `PW64_CC=clang-cl`/`clang`) and archives it as
//! `pw64game` (linked whole-archive, so every unresolved symbol shows up at
//! link time).
//!
//! - `decomp/` stays pristine: `patches/<path>.patch` (unified diff against
//!   `decomp/<path>`) is applied to a copy in OUT_DIR, which is compiled
//!   instead of the original.
//! - `native/include` shadows decomp headers; `native/pw64_native.h` is
//!   force-included into every file.
//! - `ported.txt` (functions ported to the Rust crate `pw64-kernel`): the
//!   C definition is renamed to `pw64_c_<name>` via a per-file `-D` (its own
//!   TU only, still linked for the differential tests), while every other C
//!   file keeps calling `<name>` — now provided by Rust. See
//!   docs/notes/rust-port.md.
//! - Per-object incremental: an object is rebuilt only when its source, one of
//!   its headers (clang depfile) or the flags changed.
//! - Every file's diagnostics go to `<obj>.log`; all of them are concatenated
//!   into `$OUT_DIR/c-warnings.log` and summarised in one cargo warning.
//!
//! See docs/notes/native-build.md.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

// Flag sets, source dirs, include dirs, defines and the patch applier live in
// `pw64-cbuild` (shared with the first-run builder): kept identical by
// construction. See docs/notes/first-run-build.md.
use pw64_cbuild::{DECOMP_INCLUDES, DEFINES, SRC_DIRS, apply_unified_diff, defines_at_column0};

/// Link the exe at a fixed base (no ASLR) in 0x80000000..4 GB, so every C
/// global and function address fits in the 32-bit ints/Gfx words the C code
/// stores them in (native-build.md §2, option C; the default x64 base
/// 0x140000000 is above 4 GB), and has bit 31 set: `_uvMediaCopy`/`uvMemRead`
/// treat `(u32)p & 0x80000000 == 0` as a ROM offset. 0xC0000000 stays clear of
/// the RDRAM window (0x80000000..0x80800000) and the coroutine stacks
/// (pw64-platform, 0x80800000..0xA0000000).
/// Applied to this crate's tests here; exported as `links` metadata
/// (`DEP_PW64GAME_LOW_BASE_LINK_ARGS`) for dependents' build scripts
/// (`crates/birdman64/build.rs`), since link args don't propagate.
const LOW_BASE_LINK_ARGS: &[&str] = &[
    "/BASE:0xC0000000",
    "/DYNAMICBASE:NO",
    "/HIGHENTROPYVA:NO",
    // LNK4281 "undesirable base address for x64 image": intended.
    "/IGNORE:4281",
];

/// [`LOW_BASE_LINK_ARGS`] for Linux (rust-lld, rustc's default linker for
/// x86_64-unknown-linux-gnu, driven by `cc`). rustc links PIE (`-pie`) and the
/// kernel loads a PIE anywhere, ignoring its base, so the image must be a
/// fixed ET_EXEC. `--no-pie` goes straight to lld: the `cc` driver still sees
/// `-pie` and keeps the PIC crt objects (Scrt1.o/crtbeginS.o), whereas
/// `-no-pie` would pull in crtbegin.o, whose R_X86_64_32S relocations can't
/// reach 0xC0000000 (> 2 GB). GNU ld has no ELF `--image-base`
/// (`-Ttext-segment` there); rust-lld rejects `-Ttext-segment`.
const LOW_BASE_LINK_ARGS_LINUX: &[&str] = &["-Wl,--no-pie", "-Wl,--image-base=0xC0000000"];

/// The C toolchain flavour, from the *target* (build scripts run on the host,
/// so `cfg!(windows)` would be wrong when cross-checking).
#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    /// clang-cl, x86_64-pc-windows-msvc (LLP64, MSVC bitfields, PE).
    Msvc,
    /// clang, x86_64-unknown-linux-gnu (LP64, SysV bitfields, ELF).
    Gnu,
}

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let flavor = match (target_os.as_str(), target_arch.as_str()) {
        ("windows", _) => Flavor::Msvc,
        ("linux", "x86_64") => Flavor::Gnu,
        _ => panic!(
            "pw64-game: the native C build supports Windows (clang-cl) and x86_64 Linux \
             (clang) only, not {target_arch}-{target_os}: the C needs every address below \
             4 GB with bit 31 set (fixed image base, RDRAM window) — macOS/arm64 can't map \
             there; see docs/notes/native-build.md §10"
        ),
    };
    // Crash diagnosis: when set, write a link map so a RIP from the
    // pw64-platform crash log resolves to a C symbol. It must be part of
    // the exported list, since link args don't propagate (see below).
    let map_args: Vec<String> = std::env::var("PW64_MAP")
        .map(|map| {
            println!("cargo:rerun-if-env-changed=PW64_MAP");
            vec![match flavor {
                Flavor::Msvc => format!("/MAP:{map}"),
                Flavor::Gnu => format!("-Wl,-Map={map}"),
            }]
        })
        .unwrap_or_default();
    let base_args = match flavor {
        Flavor::Msvc => LOW_BASE_LINK_ARGS,
        Flavor::Gnu => LOW_BASE_LINK_ARGS_LINUX,
    };
    let link_args: Vec<String> = base_args
        .iter()
        .map(|a| a.to_string())
        .chain(map_args)
        .collect();
    for a in &link_args {
        println!("cargo:rustc-link-arg={a}");
    }
    println!("cargo:low_base_link_args={}", link_args.join(" "));
    let manifest = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let is_static = std::env::var_os("CARGO_FEATURE_STATIC").is_some();
    let is_dylib = std::env::var_os("CARGO_FEATURE_DYLIB").is_some();
    assert!(
        is_static != is_dylib,
        "pw64-game: enable exactly one of the features `static` (default) and `dylib` \
         (for dylib: --no-default-features --features dylib)"
    );
    if is_dylib {
        // No C here: the game module is built on the player's machine
        // (docs/notes/first-run-build.md). The exe keeps the low fixed base
        // (its Rust statics are C-visible) and gets the import table.
        dylib_imports(&manifest.join("dylib/pw64_dll_imports.h"));
        dylib_abi(&manifest.join("dylib/pw64_dll_shim.c"));
        return;
    }
    let out = PathBuf::from(env("OUT_DIR"));
    let decomp = plain_path(
        &manifest
            .join("../../decomp")
            .canonicalize()
            .expect("decomp/ submodule missing"),
    );
    let native = manifest.join("native");
    let patches = manifest.join("patches");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", native.display());
    println!("cargo:rerun-if-changed={}", patches.display());
    println!(
        "cargo:rerun-if-changed={}",
        decomp.join("include").display()
    );
    for dir in SRC_DIRS {
        println!("cargo:rerun-if-changed={}", decomp.join(dir).display());
    }
    println!("cargo:rerun-if-env-changed=PW64_CC");
    println!("cargo:rerun-if-env-changed=PW64_ZIG");
    println!("cargo:rerun-if-env-changed=PW64_CLANG_CL");
    println!("cargo:rerun-if-env-changed=PW64_CLANG");

    // Which decomp file defines every ported function: that TU gets
    // `-D<name>=pw64_c_<name>` (the Rust crate provides `<name>` instead).
    let ported = ported_defs(&decomp, &patches);
    // decomp/include is mirrored into OUT_DIR with header patches applied
    // (a copy of the whole tree, so quoted includes between headers can't
    // reach an unpatched original next to them).
    let include_root = mirror_include_tree(&decomp, &out, &patches);
    let repo = manifest.join("../..");
    let cc = match std::env::var("PW64_CC").unwrap_or_default().as_str() {
        "" | "zig" => zig_cc(flavor, &find_zig(&repo), &native, &decomp, &include_root),
        "clang-cl" | "clang" => legacy_cc(flavor, &native, &decomp, &include_root),
        other => panic!("PW64_CC={other:?}: use zig (default) or clang-cl/clang (legacy LLVM)"),
    };

    // For reproducing a single compile by hand.
    fs::write(
        out.join("c-flags.txt"),
        format!("{} {}\n", cc.path.display(), cc.args.join(" ")),
    )
    .unwrap();

    let flags_hash = {
        let mut h = DefaultHasher::new();
        (&cc.path, &cc.version, &cc.args).hash(&mut h);
        format!("{:016x}", h.finish())
    };

    // Collect sources, applying patches.
    let mut jobs = Vec::new();
    for dir in SRC_DIRS {
        let mut files: Vec<PathBuf> = fs::read_dir(decomp.join(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "c"))
            .collect();
        files.sort();
        for src in files {
            let rel = format!("{dir}/{}", src.file_name().unwrap().to_string_lossy());
            let patch = patches.join(format!("{rel}.patch"));
            let src = if patch.exists() {
                apply_patch_copy(&decomp, &out, &rel, &patch)
            } else {
                src
            };
            let obj = out
                .join("obj")
                .join(rel.replace('/', "_").replace(".c", ".obj"));
            let renamed: Vec<String> = ported
                .iter()
                .filter(|(r, _)| r == &rel)
                .flat_map(|(_, f)| f.iter().cloned())
                .collect();
            jobs.push((src, obj, renamed));
        }
    }
    check_patches_used(&patches, &decomp);
    // Our own C (native/src): ROM-layout loaders that need the decomp's
    // struct definitions (e.g. the audio bank/sequence deserialisers).
    let mut own: Vec<PathBuf> = fs::read_dir(native.join("src"))
        .map(|rd| rd.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    own.retain(|p| p.extension().is_some_and(|e| e == "c"));
    own.sort();
    for src in own {
        let name = src.file_stem().unwrap().to_string_lossy().into_owned();
        let obj = out.join("obj").join(format!("native_{name}.obj"));
        jobs.push((src, obj, Vec::new()));
    }

    fs::create_dir_all(out.join("obj")).unwrap();
    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((src, obj, extra)) = jobs.get(i) else {
                        break;
                    };
                    if let Err(e) = compile_one(&cc, &flags_hash, src, obj, extra) {
                        failures.lock().unwrap().push(e);
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    if !failures.is_empty() {
        for f in &failures {
            eprintln!("{f}");
        }
        panic!("{} decomp C file(s) failed to compile", failures.len());
    }

    // Aggregate diagnostics.
    let mut log = String::new();
    for (_, obj, _) in &jobs {
        log.push_str(&fs::read_to_string(obj.with_extension("log")).unwrap_or_default());
    }
    let count = |needle: &str| log.lines().filter(|l| l.contains(needle)).count();
    let total = count(": warning:");
    let ptr = count("[-Wpointer-to-int-cast]")
        + count("[-Wint-to-pointer-cast]")
        + count("[-Wvoid-pointer-to-int-cast]")
        + count("[-Wint-to-void-pointer-cast]")
        + count("[-Wint-conversion]")
        + count("[-Wshorten-64-to-32]");
    let log_path = out.join("c-warnings.log");
    fs::write(&log_path, &log).unwrap();
    println!(
        "cargo:warning=decomp C: {} files, {total} warnings ({ptr} pointer-width); see {}",
        jobs.len(),
        log_path.display()
    );

    // Archive and link every object (whole-archive: nothing gets dropped, so
    // the link reports every unresolved symbol).
    let objs: Vec<&PathBuf> = jobs.iter().map(|(_, o, _)| o).collect();
    match &cc.legacy {
        Some(base) => {
            base.clone()
                .objects(objs)
                .link_lib_modifier("+whole-archive")
                .compile("pw64game");
        }
        None => zig_archive(&cc.path, flavor, &out, &objs),
    }
}

/// How a decomp C file is compiled: the executable, its args (everything but
/// the per-file `-c`/output/depfile/source) and the driver spelling.
struct CcCommand {
    path: PathBuf,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    args: Vec<String>,
    /// clang-cl spelling (`-Fo`, `/clang:-MD`) vs GNU (`-o`, `-MD -MF`).
    cl_style: bool,
    /// zig's version (part of the objects' rebuild key), empty for legacy.
    version: String,
    /// Legacy only: the cc builder that archives the objects (lib.exe/ar).
    legacy: Option<cc::Build>,
}

/// T13 (docs/notes/first-run-build.md): the decomp compiled with zig
/// [`pw64_cbuild::ZIG_VERSION`] and exactly the first-run builder's flag set
/// (`zig_flags`: clang 21, `-O3`, /GS strong, function sections, ...), so dev
/// builds, tests and CI run the same codegen the players' game module gets.
/// Only additions: debug info when the cargo profile has it (`-g`, CodeView on
/// Windows; no codegen change) and, per file, the ported.txt renames.
/// zig clang is a plain clang driver: GNU spelling on both targets; on
/// Windows it emits MSVC-ABI COFF (`--target=x86_64-pc-windows-msvc`) with no
/// `/DEFAULTLIB` directives, so the CRT is whatever rustc links (msvcrt, or
/// libcmt under crt-static).
fn zig_cc(
    flavor: Flavor,
    zig: &Path,
    native: &Path,
    decomp: &Path,
    include_root: &Path,
) -> CcCommand {
    let version = zig_version(zig).unwrap_or_else(|e| panic!("{}: {e}", zig.display()));
    assert!(
        version == pw64_cbuild::ZIG_VERSION,
        "{} is zig {version}, the project pins zig {}: {}",
        zig.display(),
        pw64_cbuild::ZIG_VERSION,
        GET_ZIG
    );
    let os = match flavor {
        Flavor::Msvc => pw64_cbuild::Os::Windows,
        Flavor::Gnu => pw64_cbuild::Os::Linux,
    };
    let native_s = slash(native);
    let mut args = vec!["clang".to_string()];
    for f in pw64_cbuild::zig_flags(os) {
        if let Some(entry) = f.strip_prefix("-I<tree>/") {
            // Same roots as the legacy flavour: patched include/ mirror,
            // everything else straight from decomp/.
            let root = if entry.starts_with("include") {
                include_root
            } else {
                decomp
            };
            args.push(format!("-I{}", slash(&root.join(entry))));
        } else {
            args.push(f.replace("<native>", &native_s));
        }
    }
    let debug = std::env::var("DEBUG").unwrap_or_default();
    if !matches!(debug.as_str(), "" | "false" | "0" | "none") {
        args.push("-g".into());
        if flavor == Flavor::Msvc {
            args.push("-gcodeview".into());
        }
    }
    CcCommand {
        path: zig.to_path_buf(),
        env: Vec::new(),
        args,
        cl_style: false,
        version,
        legacy: None,
    }
}

/// How to get the pinned zig (the build.rs error texts).
const GET_ZIG: &str = "run `cargo run -p pw64-cbuild --example get_zig --features fetch` \
     (downloads + sha256-checks it into tools/zig/), or set PW64_ZIG to a zig 0.16.0 binary; \
     PW64_CC=clang-cl (Windows) / PW64_CC=clang (Linux) selects the legacy LLVM build";

/// `PW64_ZIG`, else `<repo>/tools/zig/zig[.exe]` (the get_zig example's
/// output). No PATH lookup: any other zig version is refused anyway, and a
/// stray system zig must not silently become the compiler.
fn find_zig(repo: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("PW64_ZIG") {
        return PathBuf::from(p);
    }
    // Host executable (the build script runs on the host).
    let exe = if cfg!(windows) { "zig.exe" } else { "zig" };
    let p = plain_path(
        &repo
            .join("tools/zig")
            .join(exe)
            .canonicalize()
            .unwrap_or_default(),
    );
    if p.is_file() {
        return p;
    }
    panic!("zig {} not found: {GET_ZIG}", pw64_cbuild::ZIG_VERSION);
}

/// `zig version` (no further args: zig 0.16 prints its help + exits 1 on any).
fn zig_version(zig: &Path) -> Result<String, String> {
    let o = Command::new(zig)
        .arg("version")
        .output()
        .map_err(|e| format!("running `zig version`: {e}; {GET_ZIG}"))?;
    if !o.status.success() {
        return Err(format!("`zig version` failed ({}); {GET_ZIG}", o.status));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Archives the objects with zig's bundled llvm-lib (Windows: COFF archive
/// for MSVC link.exe) / llvm-ar (Linux), then emits the same link lines cc
/// would: static, whole-archive (every unresolved symbol shows at link time).
/// Arguments go through a response file (204 long OUT_DIR paths are close to
/// Windows' 32 K command-line limit).
fn zig_archive(zig: &Path, flavor: Flavor, out: &Path, objs: &[&PathBuf]) {
    let (lib, mut rsp) = match flavor {
        Flavor::Msvc => {
            let lib = out.join("pw64game.lib");
            (lib.clone(), vec![format!("/out:{}", slash(&lib))])
        }
        Flavor::Gnu => {
            let lib = out.join("libpw64game.a");
            (lib.clone(), vec!["rcs".into(), slash(&lib)])
        }
    };
    // ar appends to an existing archive: start fresh (llvm-lib overwrites).
    let _ = fs::remove_file(&lib);
    rsp.extend(objs.iter().map(|o| slash(o)));
    let rsp_path = out.join("archive.rsp");
    let text: String = rsp.iter().map(|a| format!("\"{a}\"\n")).collect();
    fs::write(&rsp_path, text).unwrap();
    let tool = match flavor {
        Flavor::Msvc => "lib",
        Flavor::Gnu => "ar",
    };
    let o = Command::new(zig)
        .arg(tool)
        .arg(format!("@{}", slash(&rsp_path)))
        .output()
        .unwrap_or_else(|e| panic!("spawning zig {tool}: {e}"));
    assert!(
        o.status.success(),
        "zig {tool} failed ({}): {}{}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static:+whole-archive=pw64game");
}

/// Forward slashes: zig wants `C:/...` on Windows; fine everywhere.
fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// The pre-T13 build (`PW64_CC=clang-cl` / `clang`): an installed LLVM with
/// cc's baseline (target, -MD / -fPIC, opt level from the cargo profile,
/// debug info) plus the clang-cl/GNU twins of the zig flags. Kept as a
/// fallback for a transition period; not what players' modules are built with.
fn legacy_cc(flavor: Flavor, native: &Path, decomp: &Path, include_root: &Path) -> CcCommand {
    let clang_cl = match flavor {
        Flavor::Msvc => find_clang_cl(),
        Flavor::Gnu => find_clang(),
    };
    let mut base = cc::Build::new();
    base.compiler(&clang_cl)
        .cargo_warnings(false)
        .warnings(false);
    let tool = base.get_compiler();
    let mut args: Vec<String> = tool
        .args()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        // cc's -W0/-W4 (clang-cl) and -w (clang): we want clang's default
        // warning set (+ WARN_FLAGS). -fPIC: replaced by GNU_FLAGS' -fPIE.
        .filter(|a| !matches!(a.as_str(), "-W0" | "-W4" | "-w" | "-fPIC"))
        .collect();
    let gen_flags = match flavor {
        Flavor::Msvc => pw64_cbuild::MSVC_FLAGS,
        Flavor::Gnu => pw64_cbuild::GNU_FLAGS,
    };
    args.extend(
        gen_flags
            .iter()
            .chain(pw64_cbuild::WARN_FLAGS)
            .map(|s| s.to_string()),
    );
    let force_include = native.join("pw64_native.h");
    match flavor {
        Flavor::Msvc => args.push(format!("-FI{}", force_include.display())),
        Flavor::Gnu => {
            args.push("-include".into());
            args.push(force_include.display().to_string());
        }
    }
    args.extend(DEFINES.iter().map(|d| format!("-D{d}")));
    args.push(format!("-I{}", native.join("include").display()));
    args.extend(DECOMP_INCLUDES.iter().map(|i| {
        let root = if i.starts_with("include") {
            include_root
        } else {
            decomp
        };
        format!("-I{}", root.join(i).display())
    }));
    CcCommand {
        path: tool.path().to_path_buf(),
        env: tool.env().to_vec(),
        args,
        cl_style: flavor == Flavor::Msvc,
        version: String::new(),
        legacy: Some(base),
    }
}

/// `dylib` feature: `$OUT_DIR/dll_imports.rs` = every `X(name)`/`C(name)` of
/// `pw64_dll_imports.h` as `(c"name", address)`, for `pw64_dll_bind`
/// (src/dylib.rs). `X(name)` is the Rust function `name`; `C(name)` is a CRT
/// function whose shim thunk is bound to pw64-platform's `pw64_crt_name`
/// (src/crt.rs). Referencing them also keeps the pw64-platform definitions
/// in the exe (nothing else there calls most of them).
fn dylib_imports(list: &Path) {
    println!("cargo:rerun-if-changed={}", list.display());
    let text = fs::read_to_string(list).unwrap();
    let names: Vec<(&str, bool)> = text
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            // (is a C(name) CRT entry, name)
            let crt = l.strip_prefix("X(").is_none();
            let body = l.strip_prefix(if crt { "C(" } else { "X(" })?;
            Some((body.strip_suffix(')')?, crt))
        })
        .collect();
    assert!(!names.is_empty(), "{}: no X(name) lines", list.display());
    let mut rs = String::from("unsafe extern \"C-unwind\" {\n");
    for (n, crt) in &names {
        if !crt {
            rs.push_str(&format!("    fn {n}();\n"));
        }
    }
    rs.push_str("}\n\n/// `pw64_dll_imports.h`, in order. `C(name)` entries point at\n/// pw64-platform's CRT replacements (`pw64_crt_*`).\npub(crate) struct Sym(pub(crate) *const core::ffi::c_void);\nunsafe impl Sync for Sym {}\npub(crate) static IMPORTS: &[(&core::ffi::CStr, Sym)] = &[\n");
    for (n, crt) in &names {
        if *crt {
            rs.push_str(&format!(
                "    (c\"{n}\", Sym(pw64_platform::crt::pw64_crt_{n} as *const core::ffi::c_void)),\n"
            ));
        } else {
            rs.push_str(&format!(
                "    (c\"{n}\", Sym({n} as *const core::ffi::c_void)),\n"
            ));
        }
    }
    rs.push_str("];\n");
    fs::write(PathBuf::from(env("OUT_DIR")).join("dll_imports.rs"), rs).unwrap();
}

/// `dylib` feature, T10: `DLL_ABI = <pkg version>-<first 16 hex of
/// sha256(pw64_dll_imports.h + pw64_dll_shim.c + kit)>` (`kit::abi`), which
/// `src/dylib.rs` reads as `env!("DLL_ABI")`. Computed from the same embedded
/// kit the player-side builder unpacks, so the exe and every module it builds
/// agree; the builder also uses it as the cache key's ABI part, so an edited
/// shim/import list moves every module to a new key and the loader refuses
/// stale ones with "built for another version".
fn dylib_abi(shim: &Path) {
    println!("cargo:rerun-if-changed={}", shim.display());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    println!(
        "cargo:rustc-env=DLL_ABI={}",
        pw64_cbuild::kit::abi(&version, pw64_cbuild::KIT)
    );
}

/// Strips the `\\?\` verbatim prefix `canonicalize` adds on Windows: in
/// verbatim paths `/` isn't a separator, which breaks `#include <PR/x.h>`.
fn plain_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

/// `ported.txt` → `[(decomp rel path, [-Dname=pw64_c_name, …])]`: for every
/// ported function, the decomp `.c` file that **defines** it (one per line,
/// `#` comments). A definition is a line at column 0 where the name is a
/// whole word followed by `(` — calls are indented, and headers (which hold
/// the declarations) are not compiled. A patch that adds or moves a
/// definition counts too: its `+` lines are scanned. Panics with a clear
/// message when a name has no definition or two (a typo, or one the Rust
/// crate can't own alone).
fn ported_defs(decomp: &Path, patches: &Path) -> Vec<(String, Vec<String>)> {
    let file = Path::new("ported.txt");
    println!("cargo:rerun-if-changed={}", file.display());
    let text = fs::read_to_string(file)
        .unwrap_or_else(|e| panic!("ported.txt: {e} (crates/pw64-game/ported.txt)"));
    let names: Vec<&str> = text
        .lines()
        .map(|l| l.split('#').next().unwrap().trim())
        .filter(|l| !l.is_empty())
        .collect();
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for name in names {
        let flag = format!("-D{name}=pw64_c_{name}");
        let mut hits: Vec<String> = Vec::new();
        for dir in SRC_DIRS {
            let mut files: Vec<PathBuf> = fs::read_dir(decomp.join(dir))
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|e| e == "c"))
                .collect();
            files.sort();
            for src in files {
                let rel = format!("{dir}/{}", src.file_name().unwrap().to_string_lossy());
                let defines = fs::read_to_string(&src)
                    .unwrap_or_default()
                    .lines()
                    .map(String::from)
                    .chain(patched_lines(patches.join(format!("{rel}.patch"))))
                    .any(|l| defines_at_column0(&l, name));
                if defines {
                    hits.push(rel);
                }
            }
        }
        match hits.as_slice() {
            [] => panic!("ported.txt: {name} is not defined in any decomp {SRC_DIRS:?} file"),
            [rel] => out.push((rel.to_string(), vec![flag])),
            many => panic!(
                "ported.txt: {name} is defined in {} files (must be exactly one): {}",
                many.len(),
                many.join(", ")
            ),
        }
    }
    out
}

/// Added lines (`+`…) of a unified-diff patch, if it exists.
fn patched_lines(patch: PathBuf) -> impl Iterator<Item = String> {
    fs::read_to_string(patch)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix('+'))
        .map(String::from)
        .collect::<Vec<_>>()
        .into_iter()
}

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"))
}

/// `PW64_CLANG_CL`, else `clang-cl` on PATH, else the default LLVM install.
fn find_clang_cl() -> PathBuf {
    if let Ok(p) = std::env::var("PW64_CLANG_CL") {
        return PathBuf::from(p);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("clang-cl.exe");
            if p.is_file() {
                return p;
            }
        }
    }
    let default = PathBuf::from(r"C:\Program Files\LLVM\bin\clang-cl.exe");
    if default.is_file() {
        return default;
    }
    panic!("clang-cl not found: install LLVM or set PW64_CLANG_CL to clang-cl.exe");
}

/// Linux: `PW64_CLANG`, else `clang` on PATH (any clang ≥ 16 with the
/// x86-64 target; `-ftrivial-auto-var-init=zero` needs 16+).
fn find_clang() -> PathBuf {
    if let Ok(p) = std::env::var("PW64_CLANG") {
        return PathBuf::from(p);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("clang");
            if p.is_file() {
                return p;
            }
        }
    }
    panic!("clang not found: install clang (e.g. apt install clang) or set PW64_CLANG");
}

fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// True if `obj` exists, was built with the same key (flags + source path), and is newer than
/// every input listed in its depfile (source + headers).
fn up_to_date(obj: &Path, key: &str) -> bool {
    let Some(obj_time) = mtime(obj) else {
        return false;
    };
    if fs::read_to_string(obj.with_extension("flags"))
        .ok()
        .as_deref()
        != Some(key)
    {
        return false;
    }
    let Ok(dep) = fs::read_to_string(obj.with_extension("d")) else {
        return false;
    };
    // Make-style depfile: "target: dep dep \\\n dep". Paths escape spaces as "\ ".
    let body = dep
        .split_once(": ")
        .map_or("", |(_, b)| b)
        .replace("\\\n", " ")
        .replace("\\\r\n", " ");
    let mut deps = Vec::new();
    let mut cur = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                cur.push(' ');
                chars.next();
            }
            ' ' | '\n' | '\r' | '\t' => {
                if !cur.is_empty() {
                    deps.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        deps.push(cur);
    }
    !deps.is_empty()
        && deps
            .iter()
            .all(|d| mtime(Path::new(d)).is_some_and(|t| t <= obj_time))
}

fn compile_one(
    cc: &CcCommand,
    flags_hash: &str,
    src: &Path,
    obj: &Path,
    extra: &[String],
) -> Result<(), String> {
    // The source path is part of the key: adding/removing a patch switches
    // between decomp/<file> and OUT_DIR/patched/<file>. The renames (`extra`)
    // change with ported.txt: flagged objects get them in the key, unflagged
    // ones keep the old key (no mass rebuild).
    let mut key = format!("{flags_hash} {}", src.display());
    if !extra.is_empty() {
        key.push(' ');
        key.push_str(&extra.join(" "));
    }
    if up_to_date(obj, &key) {
        return Ok(());
    }
    let dep = obj.with_extension("d");
    let mut cmd = Command::new(&cc.path);
    for (k, v) in &cc.env {
        cmd.env(k, v);
    }
    cmd.args(&cc.args).args(extra).arg("-c");
    if cc.cl_style {
        cmd.arg(format!("-Fo{}", obj.display()))
            .arg("/clang:-MD")
            .arg(format!("/clang:-MF{}", dep.display()));
    } else {
        cmd.arg("-o").arg(obj).arg("-MD").arg("-MF").arg(&dep);
    }
    cmd.arg(src);
    let output = cmd
        .output()
        .map_err(|e| format!("spawning {}: {e}", cc.path.display()))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    // clang-cl echoes the input file name on stdout; drop that line.
    let name = src.file_name().unwrap().to_string_lossy().into_owned();
    let text: String = text
        .lines()
        .filter(|l| l.trim() != name)
        .map(|l| format!("{l}\n"))
        .collect();
    fs::write(obj.with_extension("log"), &text).unwrap();
    if !output.status.success() {
        return Err(format!("error compiling {}:\n{text}", src.display()));
    }
    fs::write(obj.with_extension("flags"), key).unwrap();
    Ok(())
}

/// Copies `decomp/<rel>` (and its directory's headers, for quoted includes)
/// into OUT_DIR/patched and applies `patch` to it. Returns the patched path.
fn apply_patch_copy(decomp: &Path, out: &Path, rel: &str, patch: &Path) -> PathBuf {
    let src = decomp.join(rel);
    let dst = out.join("patched").join(rel);
    let dst_dir = dst.parent().unwrap();
    fs::create_dir_all(dst_dir).unwrap();
    for e in fs::read_dir(src.parent().unwrap()).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|e| e == "h") {
            copy_if_changed(&p, &dst_dir.join(p.file_name().unwrap()));
        }
    }
    let original = fs::read_to_string(&src).unwrap();
    let diff = fs::read_to_string(patch).unwrap();
    let patched =
        apply_unified_diff(&original, &diff).unwrap_or_else(|e| panic!("{}: {e}", patch.display()));
    if fs::read_to_string(&dst).ok().as_deref() != Some(patched.as_str()) {
        fs::write(&dst, patched).unwrap();
    }
    dst
}

/// Copies `decomp/include` to `$OUT_DIR/patched/include`, applying any
/// `patches/include/<path>.patch`. Files are only rewritten when their content
/// changes, so unchanged headers keep their mtime (incremental builds).
/// Returns `$OUT_DIR/patched` (the root the `include*` dirs resolve against).
fn mirror_include_tree(decomp: &Path, out: &Path, patches: &Path) -> PathBuf {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let root = out.join("patched");
    let mut files = Vec::new();
    walk(&decomp.join("include"), &mut files);
    for src in files {
        let rel = src.strip_prefix(decomp).unwrap();
        let dst = root.join(rel);
        fs::create_dir_all(dst.parent().unwrap()).unwrap();
        let patch = patches.join(format!("{}.patch", rel.display()));
        if patch.is_file() {
            let original = fs::read_to_string(&src).unwrap();
            let diff = fs::read_to_string(&patch).unwrap();
            let patched = apply_unified_diff(&original, &diff)
                .unwrap_or_else(|e| panic!("{}: {e}", patch.display()));
            if fs::read_to_string(&dst).ok().as_deref() != Some(patched.as_str()) {
                fs::write(&dst, patched).unwrap();
            }
        } else {
            copy_if_changed(&src, &dst);
        }
    }
    root
}

fn copy_if_changed(from: &Path, to: &Path) {
    let data = fs::read(from).unwrap();
    if fs::read(to).ok().as_deref() != Some(data.as_slice()) {
        fs::write(to, data).unwrap();
    }
}

/// Every patch must name an existing decomp C file in SRC_DIRS or a header
/// under `include/`.
fn check_patches_used(patches: &Path, decomp: &Path) {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "patch") {
                out.push(p);
            }
        }
    }
    let mut all = Vec::new();
    walk(patches, &mut all);
    for p in all {
        let rel = p.strip_prefix(patches).unwrap().with_extension("");
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        let dir = rel_s.rsplit_once('/').map_or("", |(d, _)| d);
        // Only `.c` files are patched under src/: a src header patch would be
        // silently ignored (unpatched TUs include the decomp copy).
        assert!(
            decomp.join(&rel).is_file()
                && ((SRC_DIRS.contains(&dir) && rel_s.ends_with(".c"))
                    || rel_s.starts_with("include/")),
            "{}: no compiled decomp source or header {rel_s}",
            p.display()
        );
    }
}
