//! Portable Git worktree helpers for isolated parallel coding-agent spaces
//! (`tachyon worktree ...` and sidebar / switcher UI actions).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::mux::model::git_info_for_dir;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
    pub is_main: bool,
    pub dirty: Option<bool>,
}

impl WorktreeEntry {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path.to_string_lossy(),
            "head": self.head,
            "branch": self.branch,
            "detached": self.detached,
            "bare": self.bare,
            "locked": self.locked,
            "prunable": self.prunable,
            "is_main": self.is_main,
            "dirty": self.dirty,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovedWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub branch_deleted: bool,
}

impl RemovedWorktree {
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path.to_string_lossy(),
            "branch": self.branch,
            "branch_deleted": self.branch_deleted,
        })
    }
}

/// Sanitize a branch name into a single safe directory slug (`feat/auth` -> `feat-auth`).
pub fn sanitize_branch_slug(branch: &str) -> Result<String, String> {
    let trimmed = branch.trim();
    if trimmed.is_empty() {
        return Err("branch name cannot be empty".into());
    }
    if trimmed.contains("..") || trimmed.chars().any(char::is_control) {
        return Err(format!("invalid branch name `{trimmed}`"));
    }
    let mut slug = String::with_capacity(trimmed.len());
    let mut prev_dash = false;
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            slug.push(ch);
            prev_dash = ch == '-';
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    let clean = slug.trim_matches(|c| c == '-' || c == '.');
    if clean.is_empty() {
        return Err(format!("invalid branch name `{trimmed}`"));
    }
    Ok(clean.to_string())
}

/// Canonical location for new managed worktrees: sibling `<repo>.worktrees/<branch-slug>`.
pub fn default_worktree_path(repo_root: &Path, branch: &str) -> Result<PathBuf, String> {
    let slug = sanitize_branch_slug(branch)?;
    let repo_name = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("repo");
    Ok(match repo_root.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => parent.join(format!("{repo_name}.worktrees")).join(slug),
        None => PathBuf::from(format!("{repo_name}-wt-{slug}")),
    })
}

/// Human-readable session name for a linked worktree (`<repo>/<slug>`).
pub fn session_name_for_worktree(repo_name: &str, branch: &str) -> String {
    let slug = sanitize_branch_slug(branch).unwrap_or_else(|_| branch.trim().to_string());
    format!("{repo_name}/{slug}")
}

/// Resolve the primary repository root (following linked worktrees back to the main repo).
pub fn repo_root(dir: &Path) -> Result<PathBuf, String> {
    if let Some(info) = git_info_for_dir(dir) {
        return Ok(info.common_repo_root);
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("failed to run git in `{}`: {e}", dir.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("`{}` is not inside a git repository", dir.display())
        } else {
            err
        });
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return Err(format!("could not resolve git root for `{}`", dir.display()));
    }
    let p = PathBuf::from(raw);
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// Parse `git worktree list --porcelain` output into structured entries.
pub fn parse_worktree_porcelain(stdout: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;

    let flush = |cur: &mut Option<WorktreeEntry>, out: &mut Vec<WorktreeEntry>| {
        if let Some(entry) = cur.take() {
            out.push(entry);
        }
    };

    for raw_line in stdout.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            flush(&mut current, &mut entries);
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            flush(&mut current, &mut entries);
            current = Some(WorktreeEntry {
                path: PathBuf::from(path),
                head: None,
                branch: None,
                detached: false,
                bare: false,
                locked: false,
                prunable: false,
                is_main: false,
                dirty: None,
            });
            continue;
        }
        let Some(ref mut entry) = current else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            entry.head = Some(head.trim().to_string());
        } else if let Some(bref) = line.strip_prefix("branch ") {
            let trimmed = bref.trim();
            let short = trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed);
            entry.branch = Some(short.to_string());
        } else if line == "detached" {
            entry.detached = true;
        } else if line == "bare" {
            entry.bare = true;
        } else if line == "locked" || line.starts_with("locked ") {
            entry.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            entry.prunable = true;
        }
    }
    flush(&mut current, &mut entries);

    if let Some(first_non_bare) = entries.iter_mut().find(|e| !e.bare) {
        first_non_bare.is_main = true;
    }
    entries
}

/// Return `Some(true)` if `dir` has uncommitted/untracked changes, `Some(false)` if clean.
pub fn is_worktree_dirty(dir: &Path) -> Option<bool> {
    if !dir.is_dir() {
        return None;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain=v1", "--ignore-submodules=dirty"])
        .output()
        .ok()?;
    out.status.success().then(|| !out.stdout.is_empty())
}

/// List all worktrees belonging to the repository containing `repo`.
pub fn list_worktrees(repo: &Path, include_dirty: bool) -> Result<Vec<WorktreeEntry>, String> {
    let root = repo_root(repo)?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("failed to run `git worktree list`: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "`git worktree list` failed".into()
        } else {
            err
        });
    }
    let mut entries = parse_worktree_porcelain(&String::from_utf8_lossy(&out.stdout));
    if include_dirty {
        for entry in &mut entries {
            if !entry.bare && entry.path.is_dir() {
                entry.dirty = is_worktree_dirty(&entry.path);
            }
        }
    }
    Ok(entries)
}

/// Create a linked worktree for `branch`, checking out an existing local branch
/// when present or creating a new branch from `base_ref` (or `HEAD`) otherwise.
pub fn create_worktree(
    repo: &Path,
    branch: &str,
    base_ref: Option<&str>,
    custom_path: Option<&Path>,
) -> Result<WorktreeEntry, String> {
    let branch_trimmed = branch.trim();
    let _ = sanitize_branch_slug(branch_trimmed)?;
    let root = repo_root(repo)?;
    let target = match custom_path {
        Some(p) => p.to_path_buf(),
        None => default_worktree_path(&root, branch_trimmed)?,
    };
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create worktree parent `{}`: {e}", parent.display()))?;
    }

    let ref_name = format!("refs/heads/{branch_trimmed}");
    let branch_exists = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["show-ref", "--verify", "--quiet", &ref_name])
        .status()
        .is_ok_and(|s| s.success());

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(&root).args(["worktree", "add"]);
    if branch_exists {
        cmd.arg(&target).arg(branch_trimmed);
    } else {
        cmd.args(["-b", branch_trimmed]).arg(&target);
        if let Some(base) = base_ref.map(str::trim).filter(|s| !s.is_empty()) {
            cmd.arg(base);
        }
    }

    let out = cmd
        .output()
        .map_err(|e| format!("failed to run `git worktree add`: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("`git worktree add` failed for `{branch_trimmed}`")
        } else {
            err
        });
    }

    let target_canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
    let list = list_worktrees(&root, false)?;
    Ok(list
        .into_iter()
        .find(|e| {
            std::fs::canonicalize(&e.path).unwrap_or_else(|_| e.path.clone()) == target_canon
                || e.branch.as_deref() == Some(branch_trimmed)
        })
        .unwrap_or(WorktreeEntry {
            path: target_canon,
            head: None,
            branch: Some(branch_trimmed.to_string()),
            detached: false,
            bare: false,
            locked: false,
            prunable: false,
            is_main: false,
            dirty: Some(false),
        }))
}

/// Find the worktree entry matching a path, canonical path, basename, or branch name.
pub fn find_worktree<'a>(
    entries: &'a [WorktreeEntry],
    target_or_branch: &str,
) -> Option<&'a WorktreeEntry> {
    let needle = target_or_branch.trim();
    if needle.is_empty() {
        return None;
    }
    let needle_path = Path::new(needle);
    let needle_canon = std::fs::canonicalize(needle_path).ok();
    entries.iter().find(|e| {
        e.path == needle_path
            || needle_canon.as_ref().is_some_and(|c| {
                std::fs::canonicalize(&e.path).ok().as_ref() == Some(c)
            })
            || e.branch.as_deref() == Some(needle)
            || e.path.file_name().and_then(|n| n.to_str()) == Some(needle)
    })
}

/// Remove a linked git worktree cleanly (refusing to remove the primary repository).
pub fn remove_worktree(
    repo: &Path,
    target_or_branch: &str,
    force: bool,
    delete_branch: bool,
) -> Result<RemovedWorktree, String> {
    let root = repo_root(repo)?;
    let entries = list_worktrees(&root, false)?;
    let entry = find_worktree(&entries, target_or_branch)
        .ok_or_else(|| format!("no worktree matching `{target_or_branch}` in {}", root.display()))?
        .clone();
    if entry.is_main {
        return Err("refusing to remove the main repository checkout".into());
    }

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(&root).args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(&entry.path);
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run `git worktree remove`: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("`git worktree remove` failed for `{}`", entry.path.display())
        } else {
            err
        });
    }

    let mut branch_deleted = false;
    if delete_branch {
        if let Some(ref b) = entry.branch {
            let flag = if force { "-D" } else { "-d" };
            let bout = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["branch", flag, b])
                .output();
            branch_deleted = bout.is_ok_and(|o| o.status.success());
        }
    }

    Ok(RemovedWorktree {
        path: entry.path,
        branch: entry.branch,
        branch_deleted,
    })
}

/// Prune stale worktree administrative metadata whose directories were removed externally.
pub fn prune_worktrees(repo: &Path) -> Result<(), String> {
    let root = repo_root(repo)?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["worktree", "prune"])
        .output()
        .map_err(|e| format!("failed to run `git worktree prune`: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "`git worktree prune` failed".into()
        } else {
            err
        });
    }
    Ok(())
}

/// Symlink-safe containment check (`candidate` equals or lies inside `root`).
pub fn path_is_inside(candidate: &Path, root: &Path) -> bool {
    if candidate == root || candidate.starts_with(root) {
        return true;
    }
    match (std::fs::canonicalize(candidate), std::fs::canonicalize(root)) {
        (Ok(c), Ok(r)) => c == r || c.starts_with(&r),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parser_and_slug_paths_work() {
        let sample = "\
worktree /repo/main
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/main.worktrees/feat-agent
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feat/agent
locked

worktree /repo/main.worktrees/detached-1
HEAD 3333333333333333333333333333333333333333
detached
prunable gitdir file points to non-existent location
";
        let list = parse_worktree_porcelain(sample);
        assert_eq!(list.len(), 3);
        assert!(list[0].is_main);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert!(!list[1].is_main);
        assert_eq!(list[1].branch.as_deref(), Some("feat/agent"));
        assert!(list[1].locked);
        assert!(list[2].detached);
        assert!(list[2].prunable);

        assert_eq!(sanitize_branch_slug("feat/auth_v2").unwrap(), "feat-auth_v2");
        assert!(sanitize_branch_slug("..").is_err());
        assert!(sanitize_branch_slug("").is_err());
        assert_eq!(
            default_worktree_path(Path::new("/Users/me/Code/Tachyon"), "feat/wt").unwrap(),
            PathBuf::from("/Users/me/Code/Tachyon.worktrees/feat-wt")
        );
        assert_eq!(session_name_for_worktree("Tachyon", "feat/wt"), "Tachyon/feat-wt");
    }

    #[test]
    fn create_list_dirty_and_remove_worktree_roundtrip() {
        let base = std::env::temp_dir().join(format!("tachyon-wt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("demo-repo");
        std::fs::create_dir_all(&repo).unwrap();

        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
                .args(args)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "# demo\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-m", "init"]);

        let created = create_worktree(&repo, "feat/wt-test", None, None).unwrap();
        assert_eq!(created.branch.as_deref(), Some("feat/wt-test"));
        assert!(created.path.is_dir());
        assert_eq!(
            created.path.file_name().and_then(|s| s.to_str()),
            Some("feat-wt-test")
        );

        let info = git_info_for_dir(&created.path).expect("git_info_for_dir");
        assert!(info.is_linked_worktree);
        assert_eq!(info.branch, "feat/wt-test");
        assert_eq!(info.repo_name, "demo-repo");

        let clean_list = list_worktrees(&repo, true).unwrap();
        let wt_clean = clean_list
            .iter()
            .find(|e| e.branch.as_deref() == Some("feat/wt-test"))
            .unwrap();
        assert_eq!(wt_clean.dirty, Some(false));

        std::fs::write(created.path.join("untracked.txt"), "dirty\n").unwrap();
        let dirty_list = list_worktrees(&repo, true).unwrap();
        let wt_dirty = dirty_list
            .iter()
            .find(|e| e.branch.as_deref() == Some("feat/wt-test"))
            .unwrap();
        assert_eq!(wt_dirty.dirty, Some(true));

        assert!(remove_worktree(&repo, "main", true, false).is_err());

        let removed = remove_worktree(&repo, "feat/wt-test", true, true).unwrap();
        assert!(removed.branch_deleted);
        assert!(!created.path.exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
