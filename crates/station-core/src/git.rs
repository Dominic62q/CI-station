//! Git state via the `git` CLI, plus evidence-based attribution of changes.

use crate::store::Store;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangedFile {
    /// Path relative to the repo/worktree root.
    pub path: String,
    /// Two-letter porcelain status, e.g. " M", "??", "R ".
    pub code: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitStatus {
    pub root: PathBuf,
    /// None when HEAD is detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub changes: Vec<ChangedFile>,
    pub worktrees: Vec<Worktree>,
}

/// Who changed a file, judged only from recorded evidence.
#[derive(Debug, Clone, PartialEq)]
pub enum Attribution {
    /// A successful Claude edit event names this file, and the file has not
    /// been modified since.
    Claude { session_id: Option<String> },
    /// Claude edited it, but the file changed again afterwards.
    ClaudeThenModified { session_id: Option<String> },
    /// No evidence either way. Not the same as "the user did it".
    Unknown,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!("git is not installed or not on PATH")
            } else {
                anyhow!("failed to run git: {e}")
            }
        })?;
    if !out.status.success() {
        return Err(anyhow!("{}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn repo_root(dir: &Path) -> Result<PathBuf> {
    if !dir.is_dir() {
        return Err(anyhow!("{} is not a directory", dir.display()));
    }
    let out = git(dir, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not inside a git repository", dir.display()))?;
    Ok(PathBuf::from(out.trim()))
}

pub fn status(dir: &Path) -> Result<GitStatus> {
    let root = repo_root(dir)?;
    let branch = git(&root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Fails on a repo with no commits yet; that is a valid state.
    let head = git(&root, &["rev-parse", "--short", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string());
    let changes = parse_porcelain_z(&git(&root, &["status", "--porcelain=v1", "-z"])?);
    let worktrees = parse_worktrees(&git(&root, &["worktree", "list", "--porcelain"])?);
    Ok(GitStatus {
        root,
        branch,
        head,
        changes,
        worktrees,
    })
}

/// One commit on the current branch.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub ts_ms: i64,
    pub subject: String,
}

/// The newest `limit` commits reachable from HEAD, newest first. An empty
/// repository has none, which is not an error.
pub fn log(dir: &Path, limit: usize) -> Result<Vec<Commit>> {
    log_rev(dir, "HEAD", limit)
}

/// Like `log`, for any revision (a branch name, `origin/main`).
pub fn log_rev(dir: &Path, rev: &str, limit: usize) -> Result<Vec<Commit>> {
    let root = repo_root(dir)?;
    let n = format!("-n{limit}");
    let raw = match git(
        &root,
        &["log", &n, "--format=%h%x1f%ct%x1f%s%x1e", rev, "--"],
    ) {
        Ok(r) => r,
        Err(_) => return Ok(Vec::new()), // no commits yet, or no such revision
    };
    Ok(parse_log(&raw))
}

/// The branch other work is measured against: `origin/HEAD`'s target, else
/// `main` or `master` if one exists locally. `None` if it cannot be known.
pub fn default_branch(dir: &Path) -> Option<String> {
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(r) = run(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        return Some(r.strip_prefix("origin/").unwrap_or(&r).to_string());
    }
    ["main", "master"]
        .into_iter()
        .find(|b| {
            run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{b}"),
            ])
            .is_some()
        })
        .map(String::from)
}

/// Commits `head` has that `base` lacks, and the reverse: `(ahead, behind)`.
pub fn ahead_behind(dir: &Path, base: &str, head: &str) -> Result<(u32, u32)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-list", "--left-right", "--count"])
        .arg(format!("{base}...{head}"))
        .output()
        .context("running git rev-list")?;
    if !out.status.success() {
        return Err(anyhow!(
            "git rev-list: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_counts(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| anyhow!("unexpected rev-list output"))
}

/// `"<behind>\t<ahead>"` as `rev-list --left-right --count base...head` prints it.
pub fn parse_counts(raw: &str) -> Option<(u32, u32)> {
    let mut it = raw.split_whitespace();
    let behind = it.next()?.parse().ok()?;
    let ahead = it.next()?.parse().ok()?;
    Some((ahead, behind))
}

pub fn parse_log(raw: &str) -> Vec<Commit> {
    raw.split('\x1e')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').splitn(3, '\x1f');
            let hash = f.next()?.trim();
            let secs: i64 = f.next()?.trim().parse().ok()?;
            let subject = f.next()?.trim_end_matches('\n');
            if hash.is_empty() {
                return None;
            }
            Some(Commit {
                hash: hash.to_string(),
                ts_ms: secs * 1000,
                subject: subject.to_string(),
            })
        })
        .collect()
}

/// Parse `git status --porcelain=v1 -z`. Rename/copy entries are followed by
/// an extra NUL-separated token holding the original path, which is skipped.
pub fn parse_porcelain_z(raw: &str) -> Vec<ChangedFile> {
    let mut out = Vec::new();
    let mut parts = raw.split('\0');
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let code = &entry[..2];
        out.push(ChangedFile {
            path: entry[3..].to_string(),
            code: code.to_string(),
        });
        if code.contains('R') || code.contains('C') {
            parts.next();
        }
    }
    out
}

pub fn parse_worktrees(raw: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in raw.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            cur = Some(Worktree {
                path: PathBuf::from(p),
                branch: None,
                head: None,
            });
        } else if let Some(w) = cur.as_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                w.head = Some(h.chars().take(7).collect());
            } else if let Some(b) = line.strip_prefix("branch ") {
                w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            }
        }
    }
    out.extend(cur);
    out
}

/// Attribute one changed file using recorded edit events and the file's
/// modification time. Never guesses: no event means `Unknown`.
pub fn attribute(store: &Store, root: &Path, change: &ChangedFile) -> Result<Attribution> {
    let abs = root.join(&change.path);
    let Some(abs_str) = abs.to_str() else {
        return Ok(Attribution::Unknown);
    };
    let Some((ts_ms, session_id)) = store.last_edit_of(abs_str)? else {
        return Ok(Attribution::Unknown);
    };
    let mtime_ms = std::fs::metadata(&abs)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    // The edit event fires just after the write, so allow a small margin.
    Ok(match mtime_ms {
        Some(m) if m > ts_ms + 2_000 => Attribution::ClaudeThenModified { session_id },
        _ => Attribution::Claude { session_id },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    #[test]
    fn counts_are_ahead_then_behind() {
        // left = base-only (behind), right = head-only (ahead)
        assert_eq!(parse_counts("2\t5\n"), Some((5, 2)));
        assert_eq!(parse_counts("x"), None);
    }

    #[test]
    fn parses_log_records() {
        let raw = "abc1234\x1f1700000000\x1ffix: a | b\x1e\ndef5678\x1f1700000100\x1fsecond\x1e\n";
        let c = parse_log(raw);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].hash, "abc1234");
        assert_eq!(c[0].ts_ms, 1_700_000_000_000);
        assert_eq!(c[0].subject, "fix: a | b");
        assert_eq!(c[1].subject, "second");
        assert!(parse_log("").is_empty());
    }

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        run(d.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(d.path().join("a.txt"), "1").unwrap();
        run(d.path(), &["add", "."]);
        run(d.path(), &["commit", "-q", "-m", "init"]);
        d
    }

    #[test]
    fn reports_branch_and_changes() {
        let d = repo();
        std::fs::write(d.path().join("a.txt"), "2").unwrap();
        std::fs::write(d.path().join("new file.txt"), "x").unwrap();
        let s = status(d.path()).unwrap();
        assert_eq!(s.branch.as_deref(), Some("main"));
        let mut paths: Vec<_> = s
            .changes
            .iter()
            .map(|c| (c.path.as_str(), c.code.as_str()))
            .collect();
        paths.sort();
        assert_eq!(paths, [("a.txt", " M"), ("new file.txt", "??")]);
        assert_eq!(s.worktrees.len(), 1);
    }

    #[test]
    fn rename_original_path_is_not_listed_as_a_change() {
        let d = repo();
        run(d.path(), &["mv", "a.txt", "b.txt"]);
        let s = status(d.path()).unwrap();
        assert_eq!(s.changes.len(), 1);
        assert_eq!(s.changes[0].path, "b.txt");
    }

    #[test]
    fn sees_linked_worktrees_and_detached_head() {
        let d = repo();
        let wt = tempfile::tempdir().unwrap();
        let wt_path = wt.path().join("feature");
        run(
            d.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt_path.to_str().unwrap(),
            ],
        );
        let s = status(d.path()).unwrap();
        assert_eq!(s.worktrees.len(), 2);
        assert!(s
            .worktrees
            .iter()
            .any(|w| w.branch.as_deref() == Some("feature")));
        run(d.path(), &["checkout", "-q", "--detach"]);
        assert_eq!(status(d.path()).unwrap().branch, None);
    }

    #[test]
    fn empty_repo_without_commits_is_valid() {
        let d = tempfile::tempdir().unwrap();
        run(d.path(), &["init", "-q", "-b", "main"]);
        let s = status(d.path()).unwrap();
        assert_eq!(s.head, None);
    }

    #[test]
    fn non_repo_and_missing_path_give_clear_errors() {
        let d = tempfile::tempdir().unwrap();
        let e = status(d.path()).unwrap_err().to_string();
        assert!(e.contains("not inside a git repository"), "{e}");
        let e = status(Path::new("/nonexistent/xyz"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a directory"), "{e}");
    }

    fn edit_event(path: &str, ts: i64) -> Event {
        serde_json::from_value(serde_json::json!({
            "v":1,"id":format!("e{ts}"),"ts_ms":ts,"event":"PostToolUse",
            "session_id":"s1","tool":"Edit","file":path,"status":"ok"
        }))
        .unwrap()
    }

    #[test]
    fn attribution_needs_evidence() {
        let d = repo();
        let root = d.path().canonicalize().unwrap();
        let store = Store::open_in_memory().unwrap();
        let abs = root.join("a.txt");
        std::fs::write(&abs, "edited").unwrap();
        let change = ChangedFile {
            path: "a.txt".into(),
            code: " M".into(),
        };

        // No event: unknown.
        assert_eq!(
            attribute(&store, &root, &change).unwrap(),
            Attribution::Unknown
        );

        // Event just after the write: Claude.
        let now = crate::store::now_ms();
        store
            .insert_event(&edit_event(abs.to_str().unwrap(), now + 100), "claude", 0)
            .unwrap();
        assert_eq!(
            attribute(&store, &root, &change).unwrap(),
            Attribution::Claude {
                session_id: Some("s1".into())
            }
        );

        // Event long before the file's last write: modified afterwards.
        let store2 = Store::open_in_memory().unwrap();
        store2
            .insert_event(
                &edit_event(abs.to_str().unwrap(), now - 60_000),
                "claude",
                0,
            )
            .unwrap();
        assert_eq!(
            attribute(&store2, &root, &change).unwrap(),
            Attribution::ClaudeThenModified {
                session_id: Some("s1".into())
            }
        );
    }
}
