//! Read-only GitHub data through the user's own `gh` CLI.
//!
//! Station never stores a token and never writes to GitHub: it runs
//! `gh api` GET requests and one GraphQL read, as the user, and parses the
//! JSON. If `gh` is missing, not signed in, or the repo has no GitHub
//! remote, the result says so and nothing is invented.
//!
//! Parsers are pure and are tested against responses captured from a real
//! public repository (see `fixtures/`). The merge-queue fixture with entries
//! is hand-written, because the repo we captured from has no merge queue;
//! it only proves the parser reads the documented shape.

use crate::git::Commit;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// `https://github.com/o/r(.git)`, `git@github.com:o/r.git`, `ssh://git@github.com/o/r`.
pub fn parse_remote(url: &str) -> Option<Repo> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(Repo {
        owner: owner.into(),
        name: name.into(),
    })
}

/// What CI says about one commit, summed over every workflow run on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiState {
    Passed,
    Failed,
    Running,
    /// Only cancelled runs: no verdict.
    Cancelled,
}

/// One workflow on a commit: its newest run's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunBrief {
    pub name: String,
    pub state: CiState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitCi {
    pub sha: String,
    pub state: CiState,
    pub passed: u32,
    pub failed: u32,
    pub running: u32,
    pub cancelled: u32,
    /// Names of workflows that failed, for the culprit strip.
    pub failed_names: Vec<String>,
    /// One entry per workflow name, newest run of each.
    pub runs: Vec<RunBrief>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub draft: bool,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub author: String,
    pub auto_merge: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub position: u32,
    pub state: String,
    pub number: u64,
    pub title: String,
}

/// Everything one refresh learned. `merge_queue` is `None` when the repo has
/// no merge queue on the branch, which is different from an empty queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub repo: Repo,
    pub branch: String,
    pub fetched_ms: i64,
    /// Newest run first; one entry per commit that has any run.
    pub commits: Vec<CommitCi>,
    /// CI on pull-request heads (runs triggered by `pull_request`), by head sha.
    pub pr_ci: Vec<CommitCi>,
    pub pulls: Vec<PullRequest>,
    pub merge_queue: Option<Vec<QueueEntry>>,
}

/// Why there is no snapshot. Shown on screen as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    NoGh,
    NotSignedIn,
    NoRemote,
    Failed(String),
}

impl Unavailable {
    pub fn text(&self) -> String {
        match self {
            Unavailable::NoGh => "GitHub: the gh CLI is not installed".into(),
            Unavailable::NotSignedIn => "GitHub: gh is not signed in (run gh auth login)".into(),
            Unavailable::NoRemote => "GitHub: this repo has no github.com remote".into(),
            Unavailable::Failed(m) => format!("GitHub: {m}"),
        }
    }
}

// ---- parsing -------------------------------------------------------------

#[derive(Deserialize)]
struct RunsDoc {
    #[serde(default)]
    workflow_runs: Vec<RunDoc>,
}

#[derive(Deserialize)]
struct RunDoc {
    name: Option<String>,
    head_sha: String,
    status: Option<String>,
    conclusion: Option<String>,
    html_url: Option<String>,
}

/// Fold workflow runs into one verdict per commit, keeping the API's order
/// (newest first). Failure beats running, running beats success, and a commit
/// with only cancelled runs has no verdict.
pub fn parse_runs(json: &str) -> Result<Vec<CommitCi>> {
    let doc: RunsDoc = serde_json::from_str(json).context("reading workflow runs")?;
    let mut order: Vec<String> = Vec::new();
    let mut by: HashMap<String, CommitCi> = HashMap::new();
    for r in doc.workflow_runs {
        let c = by.entry(r.head_sha.clone()).or_insert_with(|| {
            order.push(r.head_sha.clone());
            CommitCi {
                sha: r.head_sha.clone(),
                state: CiState::Cancelled,
                passed: 0,
                failed: 0,
                running: 0,
                cancelled: 0,
                failed_names: Vec::new(),
                runs: Vec::new(),
                url: None,
            }
        });
        let this = match (r.status.as_deref(), r.conclusion.as_deref()) {
            (Some("completed"), Some("success")) => Some(CiState::Passed),
            (Some("completed"), Some("skipped" | "neutral")) => None,
            (Some("completed"), Some("cancelled")) => Some(CiState::Cancelled),
            (Some("completed"), Some(_)) => Some(CiState::Failed),
            _ => Some(CiState::Running),
        };
        if let (Some(st), Some(n)) = (this, &r.name) {
            // Runs come newest first, so the first one seen for a workflow is its latest.
            if !c.runs.iter().any(|x| &x.name == n) {
                c.runs.push(RunBrief {
                    name: n.clone(),
                    state: st,
                });
            }
        }
        match (r.status.as_deref(), r.conclusion.as_deref()) {
            (Some("completed"), Some("success")) => c.passed += 1,
            // Skipped and neutral runs say nothing either way.
            (Some("completed"), Some("skipped" | "neutral")) => {}
            (Some("completed"), Some("cancelled")) => c.cancelled += 1,
            (Some("completed"), Some(_)) => {
                c.failed += 1;
                if let Some(n) = &r.name {
                    if !c.failed_names.contains(n) {
                        c.failed_names.push(n.clone());
                    }
                }
                if c.url.is_none() {
                    c.url = r.html_url.clone();
                }
            }
            // queued, pending, in_progress, waiting, requested...
            _ => c.running += 1,
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|s| by.remove(&s))
        .map(|mut c| {
            c.state = if c.failed > 0 {
                CiState::Failed
            } else if c.running > 0 {
                CiState::Running
            } else if c.passed > 0 {
                CiState::Passed
            } else {
                CiState::Cancelled
            };
            c
        })
        .collect())
}

#[derive(Deserialize)]
struct PullDoc {
    number: u64,
    title: String,
    #[serde(default)]
    draft: bool,
    head: RefDoc,
    base: BaseDoc,
    user: Option<UserDoc>,
    auto_merge: Option<serde_json::Value>,
}
#[derive(Deserialize)]
struct RefDoc {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}
#[derive(Deserialize)]
struct BaseDoc {
    #[serde(rename = "ref")]
    name: String,
}
#[derive(Deserialize)]
struct UserDoc {
    login: String,
}

pub fn parse_pulls(json: &str) -> Result<Vec<PullRequest>> {
    let docs: Vec<PullDoc> = serde_json::from_str(json).context("reading pull requests")?;
    Ok(docs
        .into_iter()
        .map(|p| PullRequest {
            number: p.number,
            title: p.title,
            draft: p.draft,
            head_ref: p.head.name,
            head_sha: p.head.sha,
            base_ref: p.base.name,
            author: p.user.map(|u| u.login).unwrap_or_default(),
            auto_merge: p.auto_merge.is_some_and(|v| !v.is_null()),
        })
        .collect())
}

/// `None` = the repository has no merge queue on this branch.
pub fn parse_queue(json: &str) -> Result<Option<Vec<QueueEntry>>> {
    let v: serde_json::Value = serde_json::from_str(json).context("reading merge queue")?;
    if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
        return Err(anyhow!("GraphQL error: {errs}"));
    }
    let q = &v["data"]["repository"]["mergeQueue"];
    if q.is_null() {
        return Ok(None);
    }
    let nodes = q["entries"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<QueueEntry> = nodes
        .iter()
        .filter_map(|n| {
            Some(QueueEntry {
                position: n["position"].as_u64()? as u32,
                state: n["state"].as_str().unwrap_or("").to_string(),
                number: n["pullRequest"]["number"].as_u64()?,
                title: n["pullRequest"]["title"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect();
    out.sort_by_key(|e| e.position);
    Ok(Some(out))
}

// ---- the culprit -----------------------------------------------------------

/// One commit on the main track with whatever CI said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    pub hash: String,
    pub subject: String,
    /// `None` = GitHub has no run for this commit (not "passed").
    pub ci: Option<CiState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Culprit {
    /// Index into the judged list (0 = newest).
    pub index: usize,
    /// True when every commit between it and the last green has a verdict,
    /// so it is the first red commit. False when some have no CI result: the
    /// real first-bad commit could be any of `unchecked` newer-than-green ones.
    pub certain: bool,
    /// Commits between the culprit and the last green with no CI result.
    pub unchecked: usize,
}

/// Match local commits (short hashes) to CI results (full shas).
pub fn judge(commits: &[Commit], ci: &[CommitCi]) -> Vec<Judged> {
    commits
        .iter()
        .map(|c| Judged {
            hash: c.hash.clone(),
            subject: c.subject.clone(),
            ci: ci
                .iter()
                .find(|x| !c.hash.is_empty() && x.sha.starts_with(&c.hash))
                .map(|x| x.state),
        })
        .collect()
}

/// The culprit is the oldest failing commit that is newer than the newest
/// green one. Without a green commit in the window there is no baseline, so
/// there is no culprit. Running/cancelled/no-run commits are "unchecked".
pub fn culprit(judged: &[Judged]) -> Option<Culprit> {
    let green = judged.iter().position(|j| j.ci == Some(CiState::Passed))?;
    let idx = judged[..green]
        .iter()
        .rposition(|j| j.ci == Some(CiState::Failed))?;
    let unchecked = judged[idx + 1..green]
        .iter()
        .filter(|j| j.ci != Some(CiState::Failed))
        .count();
    Some(Culprit {
        index: idx,
        certain: unchecked == 0,
        unchecked,
    })
}

// ---- fetching ---------------------------------------------------------------

fn gh(args: &[&str]) -> std::result::Result<String, Unavailable> {
    let out = Command::new("gh").args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Unavailable::NoGh
        } else {
            Unavailable::Failed(e.to_string())
        }
    })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("gh auth login") || err.contains("HTTP 401") {
        Err(Unavailable::NotSignedIn)
    } else {
        let first = err.lines().next().unwrap_or("request failed").trim();
        Err(Unavailable::Failed(first.chars().take(120).collect()))
    }
}

pub fn remote_of(dir: &Path) -> Option<Repo> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_remote(&String::from_utf8_lossy(&out.stdout))
}

/// One refresh: runs on `branch`, open PRs into it, and its merge queue.
/// The merge-queue read failing does not hide the rest; it only leaves the
/// queue unknown (`Err` for the whole call is reserved for runs failing).
pub fn fetch(repo: &Repo, branch: &str, now_ms: i64) -> std::result::Result<Snapshot, Unavailable> {
    let slug = repo.slug();
    let runs = gh(&[
        "api",
        &format!("repos/{slug}/actions/runs?per_page=60&branch={branch}"),
    ])?;
    let commits = parse_runs(&runs).map_err(|e| Unavailable::Failed(e.to_string()))?;
    // Best effort: without this, pull requests just show no CI verdict.
    let pr_ci = gh(&[
        "api",
        &format!("repos/{slug}/actions/runs?per_page=60&event=pull_request"),
    ])
    .ok()
    .and_then(|j| parse_runs(&j).ok())
    .unwrap_or_default();
    let pulls = gh(&[
        "api",
        &format!("repos/{slug}/pulls?state=open&per_page=30&base={branch}"),
    ])
    .ok()
    .and_then(|j| parse_pulls(&j).ok())
    .unwrap_or_default();
    let query = format!(
        "query{{repository(owner:\"{}\",name:\"{}\"){{mergeQueue(branch:\"{}\"){{entries(first:20){{nodes{{position state pullRequest{{number title}}}}}}}}}}}}",
        repo.owner, repo.name, branch
    );
    let merge_queue = gh(&["api", "graphql", "-f", &format!("query={query}")])
        .ok()
        .and_then(|j| parse_queue(&j).ok())
        .flatten();
    Ok(Snapshot {
        repo: repo.clone(),
        branch: branch.into(),
        fetched_ms: now_ms,
        commits,
        pr_ci,
        pulls,
        merge_queue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNS: &str = include_str!("../fixtures/gh_runs.json");
    const PULLS: &str = include_str!("../fixtures/gh_pulls.json");
    const QUEUE_NONE: &str = include_str!("../fixtures/gh_queue_none.json");
    const QUEUE_HAND: &str = include_str!("../fixtures/gh_queue_hand_written.json");

    #[test]
    fn remotes_in_every_common_form() {
        for u in [
            "https://github.com/anomalyco/rune.git",
            "https://github.com/anomalyco/rune",
            "git@github.com:anomalyco/rune.git",
            "ssh://git@github.com/anomalyco/rune.git\n",
        ] {
            assert_eq!(parse_remote(u).unwrap().slug(), "anomalyco/rune", "{u}");
        }
        assert!(parse_remote("https://gitlab.com/a/b.git").is_none());
        assert!(parse_remote("https://github.com/onlyowner").is_none());
    }

    #[test]
    fn real_runs_fold_to_one_verdict_per_commit() {
        let c = parse_runs(RUNS).unwrap();
        // 40 runs on 20 distinct commits in the captured response.
        assert_eq!(c.len(), 20);
        let total: u32 = c
            .iter()
            .map(|x| x.passed + x.failed + x.running + x.cancelled)
            .sum();
        assert!((38..=40).contains(&total), "skipped runs only: {total}");
        for x in &c {
            if x.failed > 0 {
                assert_eq!(x.state, CiState::Failed);
                assert!(!x.failed_names.is_empty());
            }
            if x.failed == 0 && x.running > 0 {
                assert_eq!(x.state, CiState::Running);
            }
        }
        assert!(c.iter().any(|x| x.state == CiState::Failed));
        assert!(c.iter().any(|x| x.state == CiState::Passed));
    }

    #[test]
    fn failure_beats_running_beats_success_and_cancelled_alone_is_no_verdict() {
        let mk = |rows: &[(&str, &str)]| {
            let runs: Vec<String> = rows
                .iter()
                .map(|(s, c)| {
                    format!(
                        r#"{{"name":"w","head_sha":"abc","status":"{s}","conclusion":{}}}"#,
                        if c.is_empty() {
                            "null".into()
                        } else {
                            format!("\"{c}\"")
                        }
                    )
                })
                .collect();
            let j = format!(r#"{{"workflow_runs":[{}]}}"#, runs.join(","));
            parse_runs(&j).unwrap()[0].state
        };
        assert_eq!(
            mk(&[("completed", "success"), ("completed", "failure")]),
            CiState::Failed
        );
        assert_eq!(
            mk(&[("completed", "success"), ("in_progress", "")]),
            CiState::Running
        );
        assert_eq!(mk(&[("completed", "success")]), CiState::Passed);
        assert_eq!(mk(&[("completed", "cancelled")]), CiState::Cancelled);
        assert_eq!(
            mk(&[("completed", "cancelled"), ("completed", "success")]),
            CiState::Passed
        );
        // timed_out and startup_failure are failures, not unknowns.
        assert_eq!(mk(&[("completed", "timed_out")]), CiState::Failed);
        // A skipped-only commit has no verdict.
        assert_eq!(mk(&[("completed", "skipped")]), CiState::Cancelled);
    }

    #[test]
    fn real_pulls_parse() {
        let p = parse_pulls(PULLS).unwrap();
        assert!(!p.is_empty());
        assert!(p[0].number > 0 && !p[0].head_sha.is_empty() && !p[0].base_ref.is_empty());
    }

    #[test]
    fn no_merge_queue_is_not_an_empty_queue() {
        assert_eq!(parse_queue(QUEUE_NONE).unwrap(), None);
        let q = parse_queue(QUEUE_HAND).unwrap().unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!((q[0].position, q[0].number), (1, 12));
        assert_eq!(q[1].state, "AWAITING_CHECKS");
        assert!(parse_queue(r#"{"errors":[{"message":"nope"}]}"#).is_err());
    }

    fn j(hash: &str, ci: Option<CiState>) -> Judged {
        Judged {
            hash: hash.into(),
            subject: String::new(),
            ci,
        }
    }

    #[test]
    fn culprit_is_the_oldest_red_commit_after_the_last_green() {
        use CiState::*;
        // newest first: red, red, green
        let v = [
            j("a", Some(Failed)),
            j("b", Some(Failed)),
            j("c", Some(Passed)),
        ];
        let c = culprit(&v).unwrap();
        assert_eq!((c.index, c.certain, c.unchecked), (1, true, 0));
    }

    #[test]
    fn culprit_is_uncertain_when_commits_between_have_no_result() {
        use CiState::*;
        let v = [
            j("a", Some(Failed)),
            j("b", None),
            j("c", Some(Running)),
            j("d", Some(Passed)),
        ];
        let c = culprit(&v).unwrap();
        assert_eq!((c.index, c.certain, c.unchecked), (0, false, 2));
    }

    #[test]
    fn no_culprit_without_a_green_baseline_or_without_a_failure() {
        use CiState::*;
        assert!(culprit(&[j("a", Some(Failed)), j("b", None)]).is_none());
        assert!(culprit(&[j("a", Some(Passed)), j("b", Some(Failed))]).is_none());
        assert!(culprit(&[j("a", None), j("b", Some(Passed))]).is_none());
    }

    #[test]
    fn judge_matches_short_hashes_to_full_shas() {
        let ci = parse_runs(RUNS).unwrap();
        let sha = ci[0].sha.clone();
        let commits = vec![
            Commit {
                hash: sha[..9].into(),
                ts_ms: 0,
                subject: "x".into(),
            },
            Commit {
                hash: "0000000".into(),
                ts_ms: 0,
                subject: "y".into(),
            },
        ];
        let v = judge(&commits, &ci);
        assert_eq!(v[0].ci, Some(ci[0].state));
        assert_eq!(v[1].ci, None);
    }
}
