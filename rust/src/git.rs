//! The two `git rev-parse` questions the CLI asks, answered by reading `.git`
//! directly in the ordinary layouts and by running git everywhere else.
//!
//! Python asks `git -C CWD rev-parse --show-toplevel` and
//! `git -C CWD rev-parse --path-format=absolute --git-common-dir`. The native
//! path covers a plain checkout and a linked worktree on one filesystem, owned
//! by this user, with no environment or config that changes discovery. Any
//! other layout (bare repositories, `core.worktree`, `GIT_DIR`, a mount point,
//! a foreign owner, a malformed `.git` file) is handed to git itself, so the
//! answers match whatever git would say. `COLLAB_NATIVE_GIT=0` forces git.

use crate::pyfmt;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DISCOVERY_ENV: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_NAMESPACE",
    "GIT_TEST_ASSUME_DIFFERENT_OWNER",
];

/// What git would report for `cwd`: `Some((toplevel, common_dir))` when
/// `cwd` is inside a worktree, `None` when it is not in a repository.
/// The outer `Option` is `None` when the layout needs git itself.
fn native(cwd: &Path) -> Option<Option<(PathBuf, PathBuf)>> {
    if std::env::var("COLLAB_NATIVE_GIT").as_deref() == Ok("0") || DISCOVERY_ENV.iter().any(|k| std::env::var_os(k).is_some()) || !env_config_harmless() {
        return None;
    }
    if !on_path("git") {
        return Some(None); // Python's subprocess raises OSError: not a repository to it
    }
    if cwd.components().any(|c| c.as_os_str() == ".git") {
        return None;
    }
    let uid = unsafe { libc::geteuid() };
    let device = fs::metadata(cwd).ok()?.dev();
    let mut dir = cwd.to_path_buf();
    loop {
        let meta = fs::metadata(&dir).ok()?;
        if meta.dev() != device {
            return Some(None); // git stops at a filesystem boundary: not a repository
        }
        let dotgit = dir.join(".git");
        match fs::symlink_metadata(&dotgit) {
            Ok(m) if m.is_dir() => {
                if !gitdir_valid(&dotgit) {
                    return None;
                }
                return finish(&dir, &dotgit, uid, &meta);
            }
            Ok(m) if m.is_file() => {
                let raw = fs::read(&dotgit).ok()?;
                let text = std::str::from_utf8(&raw).ok()?;
                let target = text.strip_prefix("gitdir: ")?.trim_end_matches(['\n', '\r']);
                if target.is_empty() || target.contains('\n') {
                    return None;
                }
                let gitdir = if target.starts_with('/') { PathBuf::from(target) } else { dir.join(target) };
                if !gitdir_valid(&gitdir) {
                    return None;
                }
                return finish(&dir, &gitdir, uid, &meta);
            }
            Ok(_) => return None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        // A directory that is itself a git directory makes git treat it as bare.
        if dir.join("HEAD").exists() && dir.join("objects").is_dir() && dir.join("refs").is_dir() {
            return None;
        }
        if !dir.pop() {
            return Some(None);
        }
    }
}

/// Whether `GIT_CONFIG_COUNT` entries (if any) leave discovery alone: only
/// settings outside `core`, `safe`, `include`, `includeIf` and `extensions`.
fn env_config_harmless() -> bool {
    let Some(count) = std::env::var_os("GIT_CONFIG_COUNT") else { return true };
    let Some(count) = count.to_str().and_then(|c| c.parse::<usize>().ok()) else { return false };
    (0..count).all(|i| match std::env::var(format!("GIT_CONFIG_KEY_{i}")) {
        Ok(key) => {
            let section = key.split('.').next().unwrap_or("").to_ascii_lowercase();
            !section.is_empty() && !["core", "safe", "include", "includeif", "extensions"].contains(&section.as_str())
        }
        Err(_) => false,
    })
}

fn gitdir_valid(gitdir: &Path) -> bool {
    let head = match fs::read(gitdir.join("HEAD")) {
        Ok(h) => h,
        Err(_) => return false,
    };
    let head_ok = head.starts_with(b"ref: refs/") || (head.len() >= 40 && head[..40].iter().all(u8::is_ascii_hexdigit));
    let common = common_dir(gitdir);
    let Some(common) = common else { return false };
    head_ok && common.join("objects").is_dir() && common.join("refs").is_dir()
}

fn common_dir(gitdir: &Path) -> Option<PathBuf> {
    match fs::read(gitdir.join("commondir")) {
        Ok(raw) => {
            let text = std::str::from_utf8(&raw).ok()?.trim_end_matches(['\n', '\r']);
            if text.is_empty() {
                return None;
            }
            Some(if text.starts_with('/') { PathBuf::from(text) } else { gitdir.join(text) })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(gitdir.to_path_buf()),
        Err(_) => None,
    }
}

fn finish(top: &Path, gitdir: &Path, uid: u32, top_meta: &fs::Metadata) -> Option<Option<(PathBuf, PathBuf)>> {
    let common = common_dir(gitdir)?;
    // safe.directory: git refuses (or a config entry allows) foreign-owned repositories.
    if top_meta.uid() != uid || fs::metadata(gitdir).ok()?.uid() != uid {
        return None;
    }
    if gitdir.join("config.worktree").exists() {
        return None;
    }
    if let Ok(config) = fs::read(common.join("config")) {
        let lower = String::from_utf8_lossy(&config).to_ascii_lowercase();
        if lower.contains("worktree") || lower.lines().any(|l| l.trim_start().starts_with("bare") && l.contains("true")) {
            return None;
        }
    }
    Some(Some((top.to_path_buf(), common)))
}

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|d| {
        let p = d.join(name);
        fs::metadata(&p).map(|m| m.is_file() && m.mode() & 0o111 != 0).unwrap_or(false)
    })
}

fn run(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.trim().to_string())
}

/// `cli._git_root` and `cli._checkout`: the repository root shared by all
/// worktrees, and this checkout's own top level, both resolved.
pub fn checkout(cwd: &Path) -> (PathBuf, PathBuf) {
    let cwd_resolved = pyfmt::resolve(cwd);
    let (top, common) = match native(&cwd_resolved) {
        Some(found) => match found {
            Some((top, common)) => (Some(top), Some(common)),
            None => (None, None),
        },
        None => (
            run(cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from),
            run(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map(PathBuf::from),
        ),
    };
    let root = match common {
        Some(c) => {
            let c = pyfmt::resolve(&c);
            if c.file_name().is_some_and(|n| n == ".git") {
                c.parent().map(Path::to_path_buf).unwrap_or(c)
            } else {
                c
            }
        }
        None => cwd_resolved.clone(),
    };
    let checkout = match top {
        Some(t) => pyfmt::resolve(&t),
        None => cwd_resolved,
    };
    (root, checkout)
}
