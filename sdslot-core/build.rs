// SPDX-License-Identifier: MIT OR Apache-2.0
//! Embed the git revision (and a dirty marker for uncommitted changes) into
//! the build, surfaced as `sdslot_core::VERSION_FULL` and shown by both the
//! CLI's --version and the GUI log. Builds without git (crates.io tarballs)
//! fall back to the plain crate version.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let full = match git(&["rev-parse", "--short=12", "HEAD"]) {
        Some(hash) => {
            // Tracked modifications only; stray untracked files aren't a
            // meaningful difference from the recorded revision.
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if dirty {
                format!("{version} (git {hash}, dirty)")
            } else {
                format!("{version} (git {hash})")
            }
        }
        None => version,
    };
    println!("cargo:rustc-env=SDSLOT_VERSION_FULL={full}");

    // Re-run when the checked-out commit or the staging state changes, so
    // the hash/dirty marker can't go stale. HEAD itself only changes on
    // checkout; a commit moves the branch ref file and appends to the
    // reflog, so watch those too. Only existing paths may be emitted — a
    // missing watched path makes cargo re-run every build.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let mut watch = vec![
            format!("{git_dir}/HEAD"),
            format!("{git_dir}/index"),
            format!("{git_dir}/packed-refs"),
            format!("{git_dir}/logs/HEAD"),
        ];
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watch.push(format!("{git_dir}/{head_ref}"));
        }
        for path in watch {
            if std::path::Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
}
