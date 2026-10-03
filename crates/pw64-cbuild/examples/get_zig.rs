//! T13: downloads the project's C compiler, zig [`ZIG_VERSION`] (clang 21
//! inside), sha256-checks the official archive and puts the zig binary +
//! its LICENSE into `tools/zig/` at the repo root (gitignored), where
//! pw64-game's build.rs looks for it. Used by devs and CI alike.
//!
//! ```sh
//! cargo run -p pw64-cbuild --example get_zig --features fetch [-- --out <dir>]
//! ```
//!
//! Does nothing when `<out>/zig[.exe] version` already prints the pinned
//! version. Windows: the zip is read with the `zip` crate; Linux: the tar.xz
//! is unpacked by the system `tar` (xz-utils), no extra crate.

use pw64_cbuild::{ZIG_VERSION, ZigDist};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The zig archive is ~100 MB; generous cap + timeout for slow links.
const MAX_BYTES: u64 = 256 << 20;

fn main() -> Result<(), String> {
    // <repo>/tools/zig, independent of the cwd.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/pw64-cbuild sits two levels below the repo root");
    let mut out = repo.join("tools").join("zig");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(it.next().ok_or("--out: missing value")?),
            other => return Err(format!("unknown argument {other:?} (only --out <dir>)")),
        }
    }
    let (dist, exe_name) = if cfg!(windows) {
        (&pw64_cbuild::ZIG_WINDOWS, "zig.exe")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        (&pw64_cbuild::ZIG_LINUX, "zig")
    } else {
        return Err("zig download: only x86_64 Windows and Linux are supported".into());
    };
    let exe = out.join(exe_name);
    if version_of(&exe).as_deref() == Some(ZIG_VERSION) {
        println!("{} is already zig {ZIG_VERSION}", exe.display());
        return Ok(());
    }
    println!("downloading {}", dist.url);
    let bytes = download(dist.url)?;
    let hash: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hash != dist.sha256 {
        return Err(format!(
            "sha256 mismatch for {}: got {hash}, expected {}",
            dist.url, dist.sha256
        ));
    }
    println!("sha256 ok ({} MB)", bytes.len() >> 20);
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    extract(dist, &bytes, &out, &exe)?;
    match version_of(&exe) {
        Some(v) if v == ZIG_VERSION => {
            println!("installed {} (zig {v})", exe.display());
            Ok(())
        }
        other => Err(format!(
            "{} does not run as zig {ZIG_VERSION} (got {other:?})",
            exe.display()
        )),
    }
}

/// `zig version` output, trimmed (spawned with no other args: zig 0.16 prints
/// its help and exits 1 on extra args).
fn version_of(exe: &Path) -> Option<String> {
    let o = Command::new(exe).arg("version").output().ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(900)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let mut last = String::new();
    for wait in [0u64, 2, 8] {
        std::thread::sleep(Duration::from_secs(wait));
        match agent.get(url).call() {
            Ok(mut r) => match r.body_mut().with_config().limit(MAX_BYTES).read_to_vec() {
                Ok(b) => return Ok(b),
                Err(e) => last = format!("reading {url}: {e}"),
            },
            Err(e) => last = format!("{url}: {e}"),
        }
        eprintln!("{last}; retrying");
    }
    Err(last)
}

/// Writes the zig binary (and the MIT LICENSE next to it) into `out`.
fn extract(dist: &ZigDist, bytes: &[u8], out: &Path, exe: &Path) -> Result<(), String> {
    let top = dist.exe_in_archive.split('/').next().unwrap();
    if cfg!(windows) {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| format!("zig zip: {e}"))?;
        for (name, dest) in [
            (dist.exe_in_archive.to_string(), exe.to_path_buf()),
            (format!("{top}/LICENSE"), out.join("LICENSE")),
        ] {
            let mut f = zip
                .by_name(&name)
                .map_err(|e| format!("zig zip: {name}: {e}"))?;
            let mut data = Vec::new();
            std::io::copy(&mut f, &mut data).map_err(|e| format!("zig zip: {name}: {e}"))?;
            std::fs::write(&dest, data).map_err(|e| format!("{}: {e}", dest.display()))?;
        }
    } else {
        let tarball = out.join("zig.tar.xz");
        std::fs::write(&tarball, bytes).map_err(|e| format!("{}: {e}", tarball.display()))?;
        let status = Command::new("tar")
            .arg("-xJf")
            .arg(&tarball)
            .arg("-C")
            .arg(out)
            .arg("--strip-components=1")
            .arg(dist.exe_in_archive)
            .arg(format!("{top}/LICENSE"))
            .status()
            .map_err(|e| format!("running tar (needs xz-utils): {e}"))?;
        let _ = std::fs::remove_file(&tarball);
        if !status.success() {
            return Err(format!("tar -xJf failed ({status})"));
        }
    }
    Ok(())
}
