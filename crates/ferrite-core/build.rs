//! The build ID (SELF-5): the git commit the firmware was built from, short, with "+" when the tree had
//! changes not committed. Read from the local repository only; "unknown" outside one.

use std::fs;
use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_in(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    let sha = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // --no-optional-locks: git status otherwise refreshes the index, which (watched below) rebuilt everything
    // after ferrite-core on every build
    let dirty = git(&["--no-optional-locks", "status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=FERRITE_BUILD={sha}{}", if dirty { "+" } else { "" });
    // Rebuild when HEAD moves or the index changes (a commit, a checkout, a staged edit).
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        for f in ["HEAD", "index", "packed-refs"] {
            println!("cargo:rerun-if-changed={dir}/{f}");
        }
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{r}");
        }
    }

    // Record and verify the sibling `rad950` repository revision against `RAD950_REV` so firmware
    // images remain reproducible while `RAD950-Mode/rad950` is consumed via a local path dependency.
    println!("cargo:rerun-if-changed=RAD950_REV");
    println!("cargo:rerun-if-env-changed=FERRITE_STRICT_RAD950_REV");
    let pinned = fs::read_to_string("RAD950_REV").ok().map(|s| s.trim().to_string()).unwrap_or_default();
    let rad_dir = Path::new("../../../rad950");
    let rad_full = git_in(rad_dir, &["rev-parse", "HEAD"]);
    let rad_short = git_in(rad_dir, &["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let rad_dirty = git_in(rad_dir, &["--no-optional-locks", "status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=RAD950_BUILD={rad_short}{}", if rad_dirty { "+" } else { "" });
    if let Some(gdir) = git_in(rad_dir, &["rev-parse", "--git-dir"]) {
        let gpath = if Path::new(&gdir).is_absolute() {
            Path::new(&gdir).to_path_buf()
        } else {
            rad_dir.join(&gdir)
        };
        for f in ["HEAD", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", gpath.join(f).display());
        }
        if let Some(r) = git_in(rad_dir, &["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={}", gpath.join(r).display());
        }
    }
    if !pinned.is_empty() {
        let mismatch = rad_full.as_deref() != Some(pinned.as_str()) || rad_dirty;
        if mismatch {
            let actual = rad_full.as_deref().unwrap_or("unknown");
            let msg = format!(
                "urad950 checkout ({actual}{}) does not match pinned RAD950_REV ({pinned})",
                if rad_dirty { "+dirty" } else { "" }
            );
            if std::env::var("FERRITE_STRICT_RAD950_REV").is_ok_and(|v| v == "1") {
                panic!("{msg}");
            } else {
                println!("cargo:warning={msg}");
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
