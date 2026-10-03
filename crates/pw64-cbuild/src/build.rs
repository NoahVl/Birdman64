//! First-run game-module builder (T7, docs/notes/first-run-build.md): the
//! player-side equivalent of `pw64-game`'s build.rs and `dylib/spike_build.sh`,
//! step by step: verified decomp tree + ops patches, one `flags.rsp`, parallel
//! `zig clang` per file (`CREATE_NO_WINDOW`, below-normal priority), the shim,
//! the link at the fixed base 0xB0000000, map + logs, then an atomic move into
//! `cache/game/<key>` with the finished module.
//!
//! The dev/static build (pw64-game build.rs) is authoritative for the flags:
//! `zig_flags` (crate root) is the same command line in GNU spelling.

use crate::ops::Op;
use crate::{Os, Progress, SRC_DIRS, Step, zig_flags, zig_tgt_common};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Windows `CreateProcess` flag so no console flashes per compile under the
/// gui-subsystem exe (docs/notes/first-run-build.md "Steps + UX").
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Windows priority class: the build happens while the player waits; zig's
/// cores must not steal from the (starting) game process.
#[cfg(windows)]
const CREATE_BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

/// Parallel compile jobs: all cores, capped at 8 to keep memory use moderate
/// (zig clang is a fresh process per file either way).
fn job_count() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8)
}

/// A build failure. Variants map to the player-facing messages in the notes
/// ("Steps + UX"); the full diagnostics live in the builder's `build.log` /
/// `c-warnings.log` and the messages reference those paths.
#[derive(Debug)]
pub enum BuildError {
    /// zig missing or blocked (often antivirus): the message names the path.
    Toolchain(String),
    /// A C file failed to compile: `(file, its diagnostics)`.
    Compile { file: String, log: String },
    /// The link failed, with its output.
    Link(String),
    /// Ops patches don't fit the verified tree (stale kit or a bug).
    Patches(String),
    /// zig ran but exited non-zero: a compile/link failure inside the
    /// builder (a bug or a broken decomp tree), not a local IO problem.
    Zig(String),
    /// The embedded kit is unusable (a build-time bug: the kit is generated
    /// and test-pinned by pw64-cbuild's own build.rs, so this should never
    /// reach a player).
    Kit(String),
    /// Local filesystem error, pre-formatted with context.
    Io(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::Toolchain(e) => write!(f, "{e}"),
            BuildError::Compile { file, log } => {
                write!(f, "compiling {file} failed:\n{}", head_lines(log, 30))
            }
            BuildError::Link(e) => write!(f, "linking the game module failed: {e}"),
            BuildError::Patches(e) => write!(f, "the game patches don't apply: {e}"),
            BuildError::Kit(e) => write!(f, "the embedded build kit is unusable: {e}"),
            BuildError::Zig(e) => write!(f, "{e}"),
            BuildError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// The first `n` lines of compiler output, plus a count of the rest: the
/// first errors name the cause, and a cascade can run to thousands of lines
/// (the full text stays in the object's `.log`).
fn head_lines(text: &str, n: usize) -> String {
    let text = text.trim_end();
    let total = text.lines().count();
    let mut s: String = text.lines().take(n).collect::<Vec<_>>().join("\n");
    if total > n {
        s.push_str(&format!("\n... ({} more lines)", total - n));
    }
    s
}

/// Everything the builder needs (see the module docs). The tree must already
/// be verified (`fetch::decomp_tree` / `fetch::verify_tree`, or a dev
/// checkout). Everything of ours (native C, shadow headers, shim, ops
/// patches) travels inside the embedded [`KIT`](crate::KIT) (T8): there is no
/// on-disk game directory at runtime.
pub struct Opts {
    /// `zig.exe` / `zig` (bundled: `<exe dir>/toolchain/zig/zig.exe`).
    pub zig: PathBuf,
    /// Verified decomp tree (pinned dirs: `src/**`, `include/**`).
    pub decomp_dir: PathBuf,
    /// Cache root: `build/<key>.tmp-<pid>` during the build, `game/<key>/`
    /// after success (first-run-build.md "Cache").
    pub out_root: PathBuf,
    /// `-DPW64_DLL_ABI` value the module reports (`pw64_dll_abi()`); the
    /// loader compares it against `dylib::DLL_ABI` (T10: the kit hash).
    pub abi: String,
    /// Extra key parts beyond the ABI, zig version and decomp commit (T10
    /// fills in the real DLL_ABI: exe version + hash of imports/shim/kit).
    pub key_extra: String,
    /// The Pilotwings64Decomp commit the tree was verified at (the manifest's
    /// `commit`): part of the build key.
    pub commit: String,
}

/// The finished module (plus what crash symbolization needs). Paths live in
/// `cache/game/<key>/` (first-run-build.md "Cache").
#[derive(Clone, Debug)]
pub struct Module {
    /// `cache/game/<key>/`.
    pub dir: PathBuf,
    /// `pw64game.dll` (Windows) / `libpw64game.so` (Linux).
    pub library: PathBuf,
    /// Link map, sibling of [`Module::library`] (`pw64game.map`): crash.rs
    /// prints `pw64game+0x<off>` from it (T14).
    pub map: PathBuf,
    /// Concatenated compiler diagnostics.
    pub warnings_log: PathBuf,
    /// Key parts, zig version, commit, warning count (plain JSON).
    pub build_json: PathBuf,
}

impl Module {
    /// The path `dylib::load` takes (`PW64_GAME_DLL`).
    pub fn library(&self) -> &Path {
        &self.library
    }
}

/// The build key (first-run-build.md "Cache"): `DLL_ABI` (= exe version +
/// hash of imports list/shim/kit) + the flags hash ([`flags_hash`]:
/// pw64-cbuild's own compile/link constants, which the ABI's kit hash does
/// not cover) + zig version + decomp commit, sanitised to path-safe
/// characters (the parts are versions/hex; anything odd becomes `_`).
pub fn build_key(key_extra: &str, zig_version: &str, commit: &str) -> String {
    sanitize(&format!(
        "{key_extra}-{}-{zig_version}-{commit}",
        flags_hash(host_os())
    ))
}

/// SHA-256 over `flags` and `link_args`, one line each, first 16 hex
/// characters. Separate from [`flags_hash`] so tests can hash a modified
/// copy of the lists.
pub(crate) fn flag_key_hash(flags: &[String], link_args: &[String]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for line in flags.iter().chain(link_args.iter()) {
        h.update(line.as_bytes());
        h.update(b"\n");
    }
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

/// Hash of pw64-cbuild's own build constants for `os`: the whole
/// [`zig_flags`](crate::zig_flags) command line (with the `<native>`/`<tree>`
/// placeholders, before substitution) plus [`hash_link_list`]: the constant
/// linker arguments and, for Linux, the exports script's text. Part of the
/// build key (first-run-build.md "Cache"): a change made only in these
/// constants would otherwise keep loading a stale cached module, because
/// the DLL_ABI kit hash does not cover them.
pub fn flags_hash(os: Os) -> String {
    flag_key_hash(&zig_flags(os), &hash_link_list(os))
}

/// [`link_args`] plus everything hashed with them: the Linux version
/// script's text (its exports are part of the module, so a change there
/// must change the key too).
fn hash_link_list(os: Os) -> Vec<String> {
    let mut link = link_args(os);
    if os == Os::Linux {
        link.push(LINUX_VERSION_SCRIPT.to_string());
    }
    link
}

/// [`flags_hash`] for the machine the launcher runs on: the game module is
/// always compiled for the host, and so is `build_key` (see [`host_os`]).
pub fn flags_hash_host() -> String {
    flags_hash(host_os())
}

/// The constant linker argument list (before the map/output/script path
/// arguments [`build_module`] appends): the Windows `lld-link` invocation
/// and the Linux `ld.lld` one, `spike_build.sh` step 4. Hashed into the
/// build key via [`flags_hash`].
pub fn link_args(os: Os) -> Vec<String> {
    match os {
        Os::Windows => [
            "lld-link",
            "-nologo",
            "-dll",
            "-noentry",
            "-nodefaultlib",
            "-machine:x64",
            "-Brepro",
            "-base:0xB0000000",
            "-fixed",
            "-dynamicbase:no",
            "-highentropyva:no",
            "-opt:ref",
            "-opt:icf",
            "-export:bootproc",
            "-export:pw64_widescreen_aspect,DATA",
            "-export:pw64_fill_screen,DATA",
            "-export:D_802B892C,DATA",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        Os::Linux => [
            "ld.lld",
            "-shared",
            "-Bsymbolic",
            "--eh-frame-hdr",
            "--image-base=0xB0000000",
            "-z",
            "noexecstack",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    }
}

/// Path-safe key characters: anything but `[A-Za-z0-9._-]` becomes `_`.
fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Runs the build. Reports [`Step::FindZig`] (0%), [`Step::ApplyPatches`]
/// (25%), [`Step::Compile`] (25-95%, per file) and [`Step::Link`] (97%); the
/// loader (T9) reports [`Step::Load`] when the module is in place.
pub fn build_module(
    opts: &Opts,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<Module, BuildError> {
    (progress)(Progress {
        step: Step::FindZig,
        fraction: 0.0,
    });
    let zig_version = match run_zig(opts, &["version".to_string()]) {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(e) => {
            return Err(BuildError::Toolchain(format!(
                "{e}; the bundled compiler (toolchain/zig/zig.exe) is missing or was blocked, \
                 often by antivirus"
            )));
        }
    };
    if zig_version.is_empty() {
        return Err(BuildError::Toolchain(format!(
            "{}: `zig version` printed nothing",
            opts.zig.display()
        )));
    }

    let key = build_key(&opts.key_extra, &zig_version, &opts.commit);
    let tmp = opts
        .out_root
        .join("build")
        .join(format!("{key}.tmp-{}", std::process::id()));
    fs_clear(&tmp)?;
    let tree = tmp.join("tree");
    let obj_dir = tmp.join("obj");
    let game = tmp.join("game");
    for d in [&tree, &obj_dir, &game] {
        std::fs::create_dir_all(d).map_err(|e| err_io(d, &e))?;
    }

    // 0. The kit (T8): our native C + shadow headers, the shim and the ops
    //    patches, extracted where zig can read them (`kit/native`, `kit/dylib`
    //    on disk; the ops stay in memory).
    let kit_entries =
        crate::kit::parse(crate::KIT).map_err(|e| BuildError::Kit(format!("embedded kit: {e}")))?;
    let kit_dir = tmp.join("kit");
    let mut ops_text: Option<&str> = None;
    for (path, data) in &kit_entries {
        if *path == crate::kit::OPS_PATH {
            ops_text = Some(
                std::str::from_utf8(data)
                    .map_err(|e| BuildError::Kit(format!("{}: {e}", crate::kit::OPS_PATH)))?,
            );
            continue;
        }
        let dst = kit_dir.join(path);
        std::fs::create_dir_all(dst.parent().unwrap()).map_err(|e| err_io(&dst, &e))?;
        std::fs::write(&dst, data).map_err(|e| err_io(&dst, &e))?;
    }
    let ops_text = ops_text
        .ok_or_else(|| BuildError::Kit(format!("embedded kit: no {}", crate::kit::OPS_PATH)))?;
    let ops: Vec<(String, Vec<Op>)> = crate::ops::from_text(ops_text)
        .map_err(|e| BuildError::Kit(format!("{}: {e}", crate::kit::OPS_PATH)))?;
    let native = kit_dir.join("native");
    let dylib = kit_dir.join("dylib");

    // 1. Source tree: pristine copy + our ops patches on top (spike_build.sh's
    //    cp -r + PATCHED overlay; GNU patch rejects our patches, our appliers
    //    run instead).
    copy_tree(&opts.decomp_dir.join("src"), &tree.join("src"))?;
    copy_tree(&opts.decomp_dir.join("include"), &tree.join("include"))?;
    apply_ops(&tree, &ops)?;
    (progress)(Progress {
        step: Step::ApplyPatches,
        fraction: 0.25,
    });

    // 2. flags.rsp: the static build's flags in GNU spelling (zig_flags),
    //    quoted, forward slashes (spaces + non-ASCII dirs are fine in it).
    let flags: Vec<String> = zig_flags(host_os())
        .iter()
        .map(|f| {
            f.replace("<native>", &slash(&native))
                .replace("<tree>", &slash(&tree))
        })
        .collect();
    let rsp = tmp.join("flags.rsp");
    std::fs::write(&rsp, response_file(&flags)).map_err(|e| err_io(&rsp, &e))?;

    // 3. Compile every decomp C + our native C in parallel.
    let ext = match host_os() {
        Os::Windows => "obj",
        Os::Linux => "o",
    };
    let mut jobs = Vec::new();
    for dir in SRC_DIRS {
        let dir_path = tree.join(dir);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir_path)
            .map_err(|e| err_io(&dir_path, &e))?
            .map(|e| e.map(|e| e.path()).map_err(|e| err_io(&dir_path, &e)))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "c"))
            .collect();
        files.sort();
        for src in files {
            let rel = format!("{dir}/{}", src.file_name().unwrap().to_string_lossy());
            let obj = obj_dir.join(format!("{}.{}", obj_stem(&rel), ext));
            jobs.push((src, obj));
        }
    }
    let native_src = native.join("src");
    let mut own: Vec<PathBuf> = std::fs::read_dir(&native_src)
        .map_err(|e| err_io(&native_src, &e))?
        .map(|e| e.map(|e| e.path()).map_err(|e| err_io(&native_src, &e)))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "c"))
        .collect();
    own.sort();
    for src in own {
        let name = src.file_stem().unwrap().to_string_lossy().into_owned();
        jobs.push((src, obj_dir.join(format!("native_{name}.{ext}"))));
    }

    let total = jobs.len();
    let next = AtomicUsize::new(0);
    // Max fraction reported so far (basis points): completion order is
    // parallel, so the raw per-file fraction would jump backwards.
    let max_bp = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    let progress = Mutex::new(&mut *progress);
    std::thread::scope(|s| {
        for _ in 0..job_count() {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((src, obj)) = jobs.get(i) else {
                        break;
                    };
                    // A panic in a worker (e.g. the progress callback printing
                    // to a closed stderr) becomes a BuildError: it must not
                    // poison the locks and cascade through the other workers.
                    let bp = 2500 + 7000 * ((i + 1).min(total)) / total;
                    let bp = max_bp.fetch_max(bp, Ordering::Relaxed).max(bp);
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        compile_one(opts, &rsp, src, obj)?;
                        let mut p = lock(&progress);
                        (**p)(Progress {
                            step: Step::Compile,
                            fraction: bp as f32 / 10000.0,
                        });
                        Ok(())
                    }));
                    let err = match r {
                        Ok(Ok(())) => continue,
                        Ok(Err(e)) => e,
                        Err(panic) => BuildError::Compile {
                            file: src.display().to_string(),
                            log: format!("build worker panicked: {}", panic_text(&panic)),
                        },
                    };
                    lock(&failures).push(err);
                }
            });
        }
    });
    let mut failures = failures
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !failures.is_empty() {
        for f in &failures {
            note(&f.to_string());
        }
        return Err(failures.remove(0));
    }

    // The shim (ours, no decomp headers): TGT + COMMON + the ABI define only,
    // like spike_build.sh.
    let mut shim_args: Vec<String> = vec!["clang".to_string()];
    shim_args.extend(zig_tgt_common(host_os()));
    shim_args.push(format!("-DPW64_DLL_ABI=\"{}\"", opts.abi));
    shim_args.push(format!("-I{}", slash(&dylib)));
    shim_args.push("-c".into());
    shim_args.push(slash(&dylib.join("pw64_dll_shim.c")));
    shim_args.push("-o".into());
    shim_args.push(slash(&obj_dir.join(format!("pw64_dll_shim.{ext}"))));
    let shim_out = run_zig(opts, &shim_args)?;
    let shim_log = obj_dir.join("pw64_dll_shim.log");
    std::fs::write(
        &shim_log,
        format!(
            "{}{}",
            String::from_utf8_lossy(&shim_out.stdout),
            String::from_utf8_lossy(&shim_out.stderr)
        ),
    )
    .map_err(|e| err_io(&shim_log, &e))?;

    // 4. Link at the fixed base (spike_build.sh step 4; no ucrtbase.lib since
    //    T1: the CRT thunks bind to the exe at load time). The constant part
    //    is in [`link_args`], the paths are appended here.
    let host = host_os();
    let library = game.join(library_name());
    let map = game.join("pw64game.map");
    let mut link_args = link_args(host);
    let mut obj_paths: Vec<PathBuf> = jobs.iter().map(|(_, o)| o.clone()).collect();
    obj_paths.push(obj_dir.join(format!("pw64_dll_shim.{ext}")));
    match host {
        Os::Windows => {
            link_args.push(format!("-map:{}", slash(&map)));
            link_args.push(format!("-out:{}", slash(&library)));
        }
        Os::Linux => {
            // -Bsymbolic + version script: every C symbol binds locally, so
            // the -fPIE objects (identical code to the static Linux build)
            // link into a .so. Nothing stays undefined since T1: the CRT
            // thunks bind via pw64_dll_bind like every other import.
            // --eh-frame-hdr: plain ld.lld omits .eh_frame_hdr/PT_GNU_EH_FRAME
            // unless asked (the cc driver always passes it); without it the
            // unwinder can't find the module's FDEs via dl_iterate_phdr and a
            // Rust panic through the C frames aborts (_URC_END_OF_STACK).
            let ver = tmp.join("exports.ver");
            std::fs::write(&ver, LINUX_VERSION_SCRIPT).map_err(|e| err_io(&ver, &e))?;
            link_args.push(format!("--version-script={}", slash(&ver)));
            link_args.push(format!("-Map={}", slash(&map)));
            link_args.push("-o".to_string());
            link_args.push(slash(&library));
        }
    }
    // Everything after the linker name goes into a response file (lld-link
    // and ld.lld both expand `@file`): 204 object paths under a cache dir
    // with the long key exceed Windows' 32 K command line ("The filename or
    // extension is too long", os error 206).
    let link_rsp = tmp.join("link.rsp");
    let rsp_args: Vec<String> = link_args
        .drain(1..)
        .chain(obj_paths.iter().map(|p| slash(p)))
        .collect();
    std::fs::write(&link_rsp, response_file(&rsp_args)).map_err(|e| err_io(&link_rsp, &e))?;
    link_args.push(format!("@{}", slash(&link_rsp)));
    let link_out = run_zig(opts, &link_args)?;
    let mut p = lock(&progress);
    (**p)(Progress {
        step: Step::Link,
        fraction: 0.97,
    });
    drop(p);

    // 5. Logs + build.json, then the atomic move into game/<key>.
    let mut warnings = String::new();
    for (_, obj) in &jobs {
        warnings.push_str(&std::fs::read_to_string(obj.with_extension("log")).unwrap_or_default());
    }
    let warning_count = warnings
        .lines()
        .filter(|l| l.contains(": warning:"))
        .count();
    let mut log = format!(
        "zig {} ({})\nkey {key}\nabi {}\ncommit {}\n{total} files, {warning_count} warnings\n",
        zig_version,
        opts.zig.display(),
        opts.abi,
        opts.commit,
    );
    log.push_str(&String::from_utf8_lossy(&link_out.stdout));
    log.push_str(&String::from_utf8_lossy(&link_out.stderr));
    let build_log = tmp.join("build.log");
    std::fs::write(&build_log, &log).map_err(|e| err_io(&build_log, &e))?;
    let warnings_log = game.join("c-warnings.log");
    std::fs::write(&warnings_log, &warnings).map_err(|e| err_io(&warnings_log, &e))?;
    let build_json = game.join("build.json");
    std::fs::write(
        &build_json,
        format!(
            "{{\n  \"key\": \"{}\",\n  \"key_extra\": \"{}\",\n  \"flags\": \"{}\",\n  \
             \"zig_version\": \"{}\",\n  \"commit\": \"{}\",\n  \"abi\": \"{}\",\n  \
             \"files\": {total},\n  \"warnings\": {warning_count}\n}}\n",
            json_escape(&key),
            json_escape(&opts.key_extra),
            json_escape(&flags_hash(host_os())),
            json_escape(&zig_version),
            json_escape(&opts.commit),
            json_escape(&opts.abi),
        ),
    )
    .map_err(|e| err_io(&build_json, &e))?;

    // Flush the module files before the rename publishes them: after a crash
    // or power loss, a renamed directory must not hold a truncated module
    // that find_cached would pick up.
    for f in [&library, &map, &build_json] {
        // Write access: Windows' FlushFileBuffers needs it.
        std::fs::OpenOptions::new()
            .write(true)
            .open(f)
            .and_then(|f| f.sync_all())
            .map_err(|e| err_io(f, &e))?;
    }

    let game_dir = opts.out_root.join("game").join(&key);
    // Installed modules only ever appear by the rename below, so an existing
    // `game/<key>` is complete: another instance built the same key
    // concurrently (or a retry after a failed load). Replace it; if Windows
    // refuses because a running instance has that module loaded, or the
    // other instance won the rename race, use its identical module.
    let installed = || module_in(&game_dir).library.is_file();
    if game_dir.exists()
        && let Err(e) = retry_fs(|| std::fs::remove_dir_all(&game_dir))
    {
        if !installed() {
            return Err(err_io(&game_dir, &e));
        }
        fs_clear(&tmp)?;
        return Ok(module_in(&game_dir));
    }
    std::fs::create_dir_all(opts.out_root.join("game")).map_err(|e| err_io(&opts.out_root, &e))?;
    if let Err(e) = retry_fs(|| std::fs::rename(&game, &game_dir))
        && !installed()
    {
        return Err(err_io(&game_dir, &e));
    }
    fs_clear(&tmp)?; // build/<key>.tmp-<pid> deleted on success
    // The paths were computed in the tmp dir: recompute them at the final
    // location (same file names, `module_in`).
    Ok(module_in(&game_dir))
}

/// Module file name in `game/<key>/` for this host.
fn library_name() -> &'static str {
    match host_os() {
        Os::Windows => "pw64game.dll",
        Os::Linux => "libpw64game.so",
    }
}

/// The [`Module`] paths inside a `game/<key>` directory.
fn module_in(dir: &Path) -> Module {
    Module {
        dir: dir.to_path_buf(),
        library: dir.join(library_name()),
        map: dir.join("pw64game.map"),
        warnings_log: dir.join("c-warnings.log"),
        build_json: dir.join("build.json"),
    }
}

/// A finished module for `key_extra` + the flags hash + `commit` already in
/// `out_root/game/`, found WITHOUT running zig (its version is the key's
/// third part): a later start must not spawn the 177 MB compiler (antivirus
/// may scan it on every launch) and must still work if the compiler was
/// removed after the build. `flags` comes from [`flags_hash_host`]: like the
/// ABI it is known without running zig, so a flags/ABI change cannot reuse
/// the older key. The newest matching directory wins.
pub fn find_cached(out_root: &Path, key_extra: &str, flags: &str, commit: &str) -> Option<Module> {
    let prefix = format!("{}-{}-", sanitize(key_extra), sanitize(flags));
    let suffix = format!("-{}", sanitize(commit));
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(out_root.join("game")).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(&prefix) && name.ends_with(&suffix)) {
            continue;
        }
        let dir = entry.path();
        let Ok(meta) = std::fs::metadata(dir.join(library_name())) else {
            continue;
        };
        let t = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
            best = Some((t, dir));
        }
    }
    best.map(|(_, dir)| module_in(&dir))
}

/// Deletes all but the newest `keep_newest` (by mtime) of the directories in
/// `entries` (`(path, mtime)` pairs), never touching `already` (e.g. the
/// module just built). Pure, so the selection is unit-tested.
fn prunable(
    entries: &[(PathBuf, std::time::SystemTime)],
    already: &Path,
    keep_newest: usize,
) -> Vec<PathBuf> {
    let mut rest: Vec<(PathBuf, std::time::SystemTime)> = entries
        .iter()
        .filter(|(p, _)| p.as_path() != already)
        .cloned()
        .collect();
    rest.sort_by_key(|&(_, m)| std::cmp::Reverse(m));
    rest.into_iter().skip(keep_newest).map(|(p, _)| p).collect()
}

/// Deletes `out_root/game/<key>` directories after a successful build
/// (first-run-build.md "Cache"): everything except `keep` and the two newest
/// other keys. A player rolling back a release, or two Birdman64 versions
/// installed side by side, can still load those; older keys are stale cache.
/// Best effort: a module another running instance has loaded can't be
/// deleted on Windows.
pub fn remove_other_keys(out_root: &Path, keep: &Path) {
    if let Ok(read) = std::fs::read_dir(out_root.join("game")) {
        let entries: Vec<(PathBuf, std::time::SystemTime)> = read
            .flatten()
            .filter(|e| e.path() != keep && e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| {
                let mtime = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (e.path(), mtime)
            })
            .collect();
        for path in prunable(&entries, keep, 2) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
    // Build dirs left by crashed or killed builds (`build/<key>.tmp-<pid>`,
    // ~20 MB each). A build takes about a minute, so anything untouched for
    // a day is not a concurrent instance's live build.
    let Ok(read) = std::fs::read_dir(out_root.join("build")) else {
        return;
    };
    let day = std::time::Duration::from_secs(24 * 3600);
    for entry in read.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > day);
        if stale && entry.file_name().to_string_lossy().contains(".tmp-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Linux exports (`pw64_dll_bind` and the loader's three entry points, `local: *`
/// keeps the -fPIE objects' symbols bound locally).
const LINUX_VERSION_SCRIPT: &str = "\
{ global: pw64_dll_bind; pw64_dll_abi; pw64_dll_range;
  bootproc; pw64_widescreen_aspect; pw64_fill_screen; D_802B892C;
  local: *; };";

/// Host target of the build ([`Os`] docs: build scripts run on the host).
fn host_os() -> Os {
    if cfg!(windows) {
        Os::Windows
    } else {
        Os::Linux
    }
}

/// zig wants `C:/...` paths on Windows; forward slashes are fine everywhere.
fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Response-file text: one quoted argument per line (`zig clang @rsp`), the
/// quotes protect spaces and non-ASCII; inner quotes are escaped (our flags
/// never contain any, but a path could). No separators: clang splits the
/// file into tokens itself, a trailing comma would join the next token.
fn response_file(flags: &[String]) -> String {
    let mut s = String::new();
    for f in flags {
        s.push('"');
        s.push_str(&f.replace('"', "\\\""));
        s.push_str("\"\n");
    }
    s
}

/// `src/kernel/bootproc.c` -> `kernel_bootproc` (spike_build.sh's object
/// naming; our native C files become `native_<stem>`).
fn obj_stem(rel: &str) -> String {
    rel.strip_prefix("src/")
        .unwrap_or(rel)
        .trim_end_matches(".c")
        .replace('/', "_")
}

/// Applies the ops patches to the tree in place (`file <path>` headers are
/// decomp-root relative). Header ops (`include/...`) and C ops (SRC_DIRS) land
/// in the one tree root, so quoted includes can't reach an unpatched original
/// (pw64-game build.rs mirrors include/ for the same reason).
fn apply_ops(tree: &Path, ops: &[(String, Vec<Op>)]) -> Result<(), BuildError> {
    for (rel, file_ops) in ops {
        if !crate::manifest::is_safe_path(rel)
            || (!rel.starts_with("src/") && !rel.starts_with("include/"))
        {
            return Err(BuildError::Patches(format!(
                "ops file {rel:?}: not under src/ or include/"
            )));
        }
        let path = tree.join(rel);
        let original = std::fs::read_to_string(&path)
            .map_err(|e| BuildError::Patches(format!("{}: {e}", path.display())))?;
        let patched = crate::ops::apply(&original, file_ops)
            .map_err(|e| BuildError::Patches(format!("{rel}: {e}")))?;
        std::fs::write(&path, patched).map_err(|e| err_io(&path, &e))?;
    }
    Ok(())
}

/// Recursive copy (skip symlinks: none live under the pinned dirs, but a
/// broken link must not abort the whole copy).
fn copy_tree(from: &Path, to: &Path) -> Result<(), BuildError> {
    std::fs::create_dir_all(to).map_err(|e| err_io(to, &e))?;
    let read = std::fs::read_dir(from).map_err(|e| err_io(from, &e))?;
    for entry in read {
        let entry = entry.map_err(|e| err_io(from, &e))?;
        let kind = entry.file_type().map_err(|e| err_io(from, &e))?;
        let dst = to.join(entry.file_name());
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            std::fs::create_dir_all(&dst).map_err(|e| err_io(&dst, &e))?;
            copy_tree(&entry.path(), &dst)?;
        } else {
            std::fs::copy(entry.path(), &dst).map_err(|e| err_io(&dst, &e))?;
        }
    }
    Ok(())
}

/// Compiles one file: `zig clang @flags.rsp -c <src> -o <obj>`, diagnostics to
/// `<obj>.log` (a zero-exit run still writes its warnings there).
fn compile_one(opts: &Opts, rsp: &Path, src: &Path, obj: &Path) -> Result<(), BuildError> {
    let args = [
        "clang".to_string(),
        format!("@{}", slash(rsp)),
        "-c".to_string(),
        slash(src),
        "-o".to_string(),
        slash(obj),
    ];
    let file = src.display().to_string();
    // run_zig turns a non-zero exit into `Zig` / `Io`, whose text names only
    // "zig clang": put the source file in front (build.log, setup screen
    // details), and keep the full output in `<obj>.log` as on success.
    let out = match run_zig(opts, &args) {
        Ok(out) => out,
        Err(BuildError::Zig(text)) => {
            let _ = std::fs::write(obj.with_extension("log"), &text);
            return Err(BuildError::Compile { file, log: text });
        }
        Err(BuildError::Io(text)) => {
            return Err(BuildError::Io(format!("compiling {file}: {text}")));
        }
        Err(e) => return Err(e),
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(obj.with_extension("log"), &text).map_err(|e| err_io(obj, &e))?;
    if !out.status.success() {
        return Err(BuildError::Compile { file, log: text });
    }
    Ok(())
}

/// Spawns zig (`CREATE_NO_WINDOW` + below-normal priority on Windows) and
/// returns its output. A non-zero exit is a `Zig` error carrying the captured
/// output (a build failure to report), unless the output names a local
/// problem the player can fix (disk full, antivirus interference): that is an
/// `Io` error, reported as the "couldn't write its files" message.
fn run_zig(opts: &Opts, args: &[String]) -> Result<Output, BuildError> {
    let mut cmd = Command::new(&opts.zig);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_BELOW_NORMAL_PRIORITY_CLASS);
    }
    let out = cmd
        .output()
        .map_err(|e| BuildError::Io(format!("running {} {args:?}: {e}", opts.zig.display())))?;
    if out.status.success() {
        return Ok(out);
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if zig_output_names_io_problem(&text) {
        Err(BuildError::Io(format!(
            "{} {} failed: {}{}",
            opts.zig.display(),
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )))
    } else {
        Err(BuildError::Zig(format!(
            "{} {} failed (exit {}): {}{}",
            opts.zig.display(),
            args.first().map(String::as_str).unwrap_or(""),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )))
    }
}

/// Whether a failed zig run's output names a local problem the player can
/// fix (a full disk, a file locked by another process): it then reports as
/// the IO-failure message instead of a report-this build failure.
fn zig_output_names_io_problem(output: &str) -> bool {
    const NEEDLES: [&str; 4] = [
        "no space left",
        "not enough space",
        "access is denied",
        "being used by another process",
    ];
    let lower = output.to_ascii_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Retries `op` for about 8 s when the failure is a transient file lock
/// ([`transient_kind`]: Windows access denied / sharing violation, Linux
/// EBUSY / ETXTBSY / EAGAIN); any other error returns at once. Antivirus scanning a freshly written file, or a running instance holding
/// the just-built module, lets go a moment later; the backoff (0.25 s
/// doubling to 2 s) keeps the total wait bounded.
pub(crate) fn retry_fs<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut wait = Duration::from_millis(250);
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut err = match op() {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    while transient_io(&err) && Instant::now() < deadline {
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_secs(2));
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => err = e,
        }
    }
    Err(err)
}

/// Whether an IO error may go away on retry (a lock that will be released).
fn transient_io(e: &std::io::Error) -> bool {
    transient_kind(e.kind(), e.raw_os_error(), cfg!(windows))
}

/// [`transient_io`] as a pure fn of the error (testable for both platforms).
/// Windows: access denied (antivirus holding a fresh file, a delete-pending
/// file) and sharing / lock violations (ERROR_SHARING_VIOLATION 32,
/// ERROR_LOCK_VIOLATION 33) clear up a moment later. Elsewhere access denied
/// is a real permission problem: only EBUSY, ETXTBSY (a running instance
/// executing the module) and EAGAIN are worth waiting for. Everything else
/// (NotFound, read-only FS, disk full) fails at once.
fn transient_kind(kind: std::io::ErrorKind, raw: Option<i32>, windows: bool) -> bool {
    use std::io::ErrorKind as K;
    if windows {
        kind == K::PermissionDenied || matches!(raw, Some(32) | Some(33))
    } else {
        matches!(
            kind,
            K::ResourceBusy | K::ExecutableFileBusy | K::WouldBlock
        )
    }
}

/// Deletes `p` if it exists (build tmp dirs are per-pid, cleared on rerun and
/// after success).
fn fs_clear(p: &Path) -> Result<(), BuildError> {
    match retry_fs(|| std::fs::remove_dir_all(p)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err_io(p, &e)),
    }
}

/// Locks `m`, recovering from poisoning: a panicked worker already became a
/// BuildError, the data behind these locks stays usable.
fn lock<T: ?Sized>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The message of a caught panic payload.
fn panic_text(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)")
}

/// Best-effort stderr line: `eprintln!` panics when stderr is closed (a
/// gui-subsystem exe, a closed pipe), which must never fail a build.
fn note(s: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{s}");
}

fn err_io(p: &Path, e: &std::io::Error) -> BuildError {
    BuildError::Io(format!("{}: {e}", p.display()))
}

/// Minimal JSON string escape (build.json values are paths, versions, ids).
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_key_is_path_safe_and_touched_by_every_part() {
        let commit = "a".repeat(40);
        let base = build_key("0.1.0-ab12cd34ef567890", "0.16.0", &commit);
        assert!(
            base.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        );
        assert_eq!(base, build_key("0.1.0-ab12cd34ef567890", "0.16.0", &commit));
        assert_ne!(base, build_key("other", "0.16.0", &commit));
        assert_ne!(base, build_key("0.1.0-ab12cd34ef567890", "0.17.0", &commit));
        assert_ne!(
            base,
            build_key("0.1.0-ab12cd34ef567890", "0.16.0", &"b".repeat(40))
        );
        // The flags hash is a key part of its own: the key is not just the
        // old `key_extra-zig-commit` concatenation.
        assert_ne!(
            base,
            sanitize(&format!("0.1.0-ab12cd34ef567890-0.16.0-{commit}"))
        );
    }

    #[test]
    fn flags_hash_changes_with_the_flag_list() {
        // The real constants hash through the same path as a modified copy.
        let flags = zig_flags(Os::Windows);
        let win = flag_key_hash(&flags, &hash_link_list(Os::Windows));
        assert_eq!(win, flags_hash(Os::Windows));
        // One changed compile flag ...
        let mut modified = flags.clone();
        modified.push("-fsome-new-flag".into());
        assert_ne!(win, flag_key_hash(&modified, &hash_link_list(Os::Windows)));
        // ... one changed link argument ...
        let mut link = hash_link_list(Os::Windows);
        link.push("-newarg".into());
        assert_ne!(win, flag_key_hash(&flags, &link));
        // ... and the Linux set (incl. the exports script text) is hashed
        // independently but covered too.
        let lin = flag_key_hash(&zig_flags(Os::Linux), &hash_link_list(Os::Linux));
        assert_eq!(lin, flags_hash(Os::Linux));
        assert_ne!(win, lin);
        let mut script = hash_link_list(Os::Linux);
        let n = script.len();
        script[n - 1].push('x'); // one byte added to the version script
        assert_ne!(lin, flag_key_hash(&zig_flags(Os::Linux), &script));
    }

    #[test]
    fn find_cached_matches_abi_flags_and_commit_any_zig() {
        let root = std::env::temp_dir().join(format!("pw64-find-cached-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let commit = "c".repeat(40);
        let flags = flags_hash(host_os());
        let mk = |key: &str, lib: bool| {
            let d = root.join("game").join(key);
            std::fs::create_dir_all(&d).unwrap();
            if lib {
                std::fs::write(d.join(library_name()), b"x").unwrap();
            }
            d
        };
        assert!(find_cached(&root, "0.1.0-aa", &flags, &commit).is_none());
        mk(&build_key("0.1.0-bb", "0.16.0", &commit), true); // other ABI
        mk(&build_key("0.1.0-aa", "0.16.0", &"d".repeat(40)), true); // other commit
        mk(&build_key("0.1.0-aa", "0.15.0", &commit), false); // no module file
        assert!(find_cached(&root, "0.1.0-aa", &flags, &commit).is_none());
        let hit = mk(&build_key("0.1.0-aa", "0.17.0", &commit), true);
        let m = find_cached(&root, "0.1.0-aa", &flags, &commit).unwrap();
        assert_eq!(m.library, hit.join(library_name()));
        // A module built with different flag/link constants does not match,
        // even at the same ABI and commit.
        mk(&build_key("0.1.0-aa", "0.16.0", &commit), true);
        assert!(find_cached(&root, "0.1.0-aa", "other-flags", &commit).is_none());
        remove_other_keys(&root, &hit);
        // The prune keeps `keep` and the two newest other keys (F8).
        let left: Vec<_> = std::fs::read_dir(root.join("game")).unwrap().collect();
        assert_eq!(left.len(), 3);
        assert!(hit.is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prune_keeps_the_newest_two() {
        let mk = |n: &str, t: u64| {
            (
                PathBuf::from(n),
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t),
            )
        };
        let keep = PathBuf::from("keep");
        let entries = vec![mk("a", 10), mk("b", 30), mk("c", 20), mk("keep", 1)];
        let mut got = prunable(&entries, &keep, 2);
        got.sort();
        // b (30) and c (20) are the two newest others; a goes.
        assert_eq!(got, [PathBuf::from("a")]);
        // Fewer than keep_newest others: nothing to delete.
        assert!(prunable(&entries, &keep, 10).is_empty());
    }

    #[test]
    fn zig_failure_classification() {
        // Local problems the player can fix read as IO failures ...
        for text in [
            "error: No space left on device",
            "clang: error: linker command failed: not enough space for output",
            "lld-link: error: could not open pw64game.dll: Access is denied",
            "The process cannot access the file because it is being used by another process.",
        ] {
            assert!(zig_output_names_io_problem(text), "{text}");
        }
        // ... everything else is a build failure to report.
        for text in [
            "error: expected ';' after expression",
            "lld-link: error: undefined symbol: bootproc",
        ] {
            assert!(!zig_output_names_io_problem(text), "{text}");
        }
    }

    #[test]
    fn transient_io_classification() {
        use std::io::ErrorKind as K;
        // Windows: access denied and sharing/lock violations wait.
        assert!(transient_kind(K::PermissionDenied, Some(5), true));
        assert!(transient_kind(K::Other, Some(32), true));
        assert!(transient_kind(K::Other, Some(33), true));
        assert!(!transient_kind(K::NotFound, Some(2), true));
        assert!(!transient_kind(K::StorageFull, Some(112), true));
        // Linux: access denied is permanent; busy files wait.
        assert!(!transient_kind(K::PermissionDenied, Some(13), false));
        assert!(!transient_kind(K::NotFound, Some(2), false));
        assert!(!transient_kind(K::ReadOnlyFilesystem, Some(30), false));
        assert!(transient_kind(K::ResourceBusy, Some(16), false));
        assert!(transient_kind(K::ExecutableFileBusy, Some(26), false));
        assert!(transient_kind(K::WouldBlock, Some(11), false));
        // The kinds the OS actually maps those errnos to.
        #[cfg(unix)]
        for errno in [16, 26, 11] {
            assert!(transient_io(&std::io::Error::from_raw_os_error(errno)));
        }
        #[cfg(unix)]
        assert!(!transient_io(&std::io::Error::from_raw_os_error(13)));
        assert!(!transient_io(&std::io::Error::from(K::NotFound)));
        // Windows sharing violations (32/33) are transient; other raw
        // errors are not.
        #[cfg(windows)]
        {
            assert!(transient_io(&std::io::Error::from_raw_os_error(32)));
            assert!(transient_io(&std::io::Error::from_raw_os_error(33)));
            assert!(!transient_io(&std::io::Error::from_raw_os_error(2)));
        }
    }

    #[test]
    fn compile_error_names_file_and_caps_output() {
        let log: String = (1..=100).map(|i| format!("err {i}\n")).collect();
        let e = BuildError::Compile {
            file: "src/app/foo.c".into(),
            log,
        };
        let s = e.to_string();
        assert!(s.starts_with("compiling src/app/foo.c failed:\nerr 1\n"));
        assert!(s.contains("err 30\n... (70 more lines)"));
        assert!(!s.contains("err 31"));
        assert_eq!(head_lines("a\nb\n", 30), "a\nb");
    }

    #[test]
    fn obj_stem_matches_spike_naming() {
        assert_eq!(obj_stem("src/kernel/bootproc.c"), "kernel_bootproc");
        assert_eq!(
            obj_stem("src/libultra/audio/synthesis.c"),
            "libultra_audio_synthesis"
        );
    }

    #[test]
    fn response_file_quotes_every_arg() {
        let rsp = response_file(&["-I\"tree\" x".into(), "-Dx=1".into()]);
        assert_eq!(rsp, "\"-I\\\"tree\\\" x\"\n\"-Dx=1\"\n");
    }
}
