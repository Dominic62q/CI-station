//! The "integration" half of the Pipeline view: the main track with CI
//! verdicts, what is waiting to be merged, and the culprit strip.
//!
//! `assemble` is pure and tested. `load` does the I/O (git, then GitHub via
//! `gh`). Every verdict here is something GitHub reported; a commit it has
//! no run for shows no verdict, never a green one.

use crate::git::{self, Commit};
use crate::github::{self, CiState, Culprit, RunBrief, Snapshot, Unavailable};
use anyhow::Result;
use std::path::Path;

/// Commits shown on the main track.
pub const MAIN_COMMITS: usize = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiLink {
    NotChecked,
    /// The reason is shown on screen.
    Unavailable(String),
    Live {
        slug: String,
        branch: String,
        fetched_ms: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainDot {
    pub hash: String,
    pub subject: String,
    pub ts_ms: i64,
    /// `None` = no CI run found for this commit.
    pub ci: Option<CiState>,
    pub failed_names: Vec<String>,
    /// One verdict per workflow on this commit.
    pub runs: Vec<RunBrief>,
}

/// Work waiting to land on the main track: a pull request, a merge-queue
/// entry, or a local worktree branch that is ahead of main.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// `#486` for a pull request, else the branch name.
    pub label: String,
    pub title: String,
    pub branch: String,
    pub pr: Option<u64>,
    pub draft: bool,
    pub auto_merge: bool,
    pub ci: Option<CiState>,
    /// Real merge-queue position and state, when the repo has a merge queue.
    pub queue: Option<(u32, String)>,
    /// Commits ahead of main in the local worktree for this branch.
    pub ahead: Option<u32>,
    /// Folder name of the local worktree that builds it.
    pub worktree: Option<String>,
}

/// A linked git worktree on its own branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub name: String,
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub link: CiLink,
    pub base: Option<String>,
    /// Newest first.
    pub main: Vec<MainDot>,
    pub culprit: Option<Culprit>,
    pub handoffs: Vec<Handoff>,
    /// Linked worktrees on their own branch: the builders git can see.
    pub worktrees: Vec<WorktreeInfo>,
    /// `Some(true)` the repo has a merge queue on the base branch,
    /// `Some(false)` it does not, `None` not known (GitHub not read).
    pub merge_queue: Option<bool>,
    pub loaded_ms: i64,
}

impl Board {
    pub fn empty() -> Board {
        Board {
            link: CiLink::NotChecked,
            base: None,
            main: Vec::new(),
            culprit: None,
            handoffs: Vec::new(),
            worktrees: Vec::new(),
            merge_queue: None,
            loaded_ms: 0,
        }
    }
}

pub fn assemble(
    commits: &[Commit],
    base: Option<&str>,
    worktrees: &[WorktreeInfo],
    gh: Option<&std::result::Result<Snapshot, Unavailable>>,
    now_ms: i64,
) -> Board {
    let snap = gh.and_then(|r| r.as_ref().ok());
    let link = match gh {
        None => CiLink::NotChecked,
        Some(Err(u)) => CiLink::Unavailable(u.text()),
        Some(Ok(s)) => CiLink::Live {
            slug: s.repo.slug(),
            branch: s.branch.clone(),
            fetched_ms: s.fetched_ms,
        },
    };
    let ci_of = |sha: &str| {
        snap.and_then(|s| {
            s.commits
                .iter()
                .chain(s.pr_ci.iter())
                .find(|c| c.sha == sha)
        })
    };

    let judged = github::judge(commits, snap.map(|s| s.commits.as_slice()).unwrap_or(&[]));
    let main: Vec<MainDot> = commits
        .iter()
        .zip(&judged)
        .map(|(c, j)| {
            let found = snap.and_then(|s| s.commits.iter().find(|x| x.sha.starts_with(&c.hash)));
            MainDot {
                hash: c.hash.clone(),
                subject: c.subject.clone(),
                ts_ms: c.ts_ms,
                ci: j.ci,
                failed_names: found.map(|x| x.failed_names.clone()).unwrap_or_default(),
                runs: found.map(|x| x.runs.clone()).unwrap_or_default(),
            }
        })
        .collect();
    let culprit = snap.and_then(|_| github::culprit(&judged));

    let mut handoffs: Vec<Handoff> = Vec::new();
    if let Some(s) = snap {
        let queue_of = |n: u64| {
            s.merge_queue
                .as_ref()
                .and_then(|q| q.iter().find(|e| e.number == n))
                .map(|e| (e.position, e.state.clone()))
        };
        for p in &s.pulls {
            let wt = worktrees.iter().find(|w| w.branch == p.head_ref);
            handoffs.push(Handoff {
                label: format!("#{}", p.number),
                title: p.title.clone(),
                branch: p.head_ref.clone(),
                pr: Some(p.number),
                draft: p.draft,
                auto_merge: p.auto_merge,
                ci: ci_of(&p.head_sha).map(|c| c.state),
                queue: queue_of(p.number),
                ahead: wt.map(|w| w.ahead),
                worktree: wt.map(|w| w.name.clone()),
            });
        }
        // Entries in the merge queue that are not in the open list (already
        // removed from the pull list by the queue) still belong on screen.
        if let Some(q) = &s.merge_queue {
            for e in q {
                if !handoffs.iter().any(|h| h.pr == Some(e.number)) {
                    handoffs.push(Handoff {
                        label: format!("#{}", e.number),
                        title: e.title.clone(),
                        branch: String::new(),
                        pr: Some(e.number),
                        draft: false,
                        auto_merge: false,
                        ci: None,
                        queue: Some((e.position, e.state.clone())),
                        ahead: None,
                        worktree: None,
                    });
                }
            }
        }
        // Queue entries first, in queue order; the rest keep GitHub's order.
        handoffs.sort_by_key(|h| h.queue.as_ref().map_or(u32::MAX, |q| q.0));
    }
    for w in worktrees {
        if w.ahead > 0 && !handoffs.iter().any(|h| h.branch == w.branch) {
            handoffs.push(Handoff {
                label: w.branch.clone(),
                title: format!("{} commit(s) ahead, no pull request", w.ahead),
                branch: w.branch.clone(),
                pr: None,
                draft: false,
                auto_merge: false,
                ci: None,
                queue: None,
                ahead: Some(w.ahead),
                worktree: Some(w.name.clone()),
            });
        }
    }

    Board {
        link,
        base: base.map(String::from),
        main,
        culprit,
        handoffs,
        worktrees: worktrees.to_vec(),
        merge_queue: snap.map(|s| s.merge_queue.is_some()),
        loaded_ms: now_ms,
    }
}

/// Steps in the made-up scenario `demo` plays.
pub const DEMO_STEPS: usize = 6;

/// A made-up board for looking at the whole flow when nothing real is changing. `step` walks a
/// small story: 0 start, 1 a pull request appears, 2 one's CI passes, 3 another's CI fails, 4 a
/// commit lands on main, 5 its CI passes. The caller must label it synthetic on screen.
pub fn demo(now_ms: i64, step: usize) -> Board {
    use crate::github::{CommitCi, PullRequest, QueueEntry, Repo};
    let mut hashes = vec![
        "e41c9a0", "b7d2f13", "9a05c6e", "3f8b21d", "c1e7740", "5d92ab8", "07fe3c5", "a86b1d2",
    ];
    if step >= 4 {
        hashes.insert(0, "d02aa51");
    }
    let commits: Vec<Commit> = hashes
        .iter()
        .enumerate()
        .map(|(i, h)| Commit {
            hash: (*h).into(),
            ts_ms: now_ms - 3_600_000 * (i as i64 + 1),
            subject: format!("demo commit {}", i + 1),
        })
        .collect();
    let ci = |sha: &str, state: CiState, failed: &[&str]| CommitCi {
        sha: format!("{sha}0000000000"),
        state,
        passed: u32::from(state == CiState::Passed),
        failed: failed.len() as u32,
        running: u32::from(state == CiState::Running),
        cancelled: 0,
        failed_names: failed.iter().map(|s| s.to_string()).collect(),
        runs: vec![
            RunBrief {
                name: "unit tests".into(),
                state,
            },
            RunBrief {
                name: "lint".into(),
                state: CiState::Passed,
            },
            RunBrief {
                name: "build".into(),
                state: if state == CiState::Running {
                    CiState::Running
                } else {
                    CiState::Passed
                },
            },
        ],
        url: None,
    };
    let mut main_ci = vec![
        ci("e41c9a0", CiState::Running, &[]),
        ci("b7d2f13", CiState::Failed, &["unit tests"]),
        // 9a05c6e has no run on purpose.
        ci("3f8b21d", CiState::Failed, &["unit tests", "lint"]),
        ci("c1e7740", CiState::Passed, &[]),
        ci("5d92ab8", CiState::Passed, &[]),
        ci("07fe3c5", CiState::Passed, &[]),
    ];
    if step >= 4 {
        let st = if step >= 5 {
            CiState::Passed
        } else {
            CiState::Running
        };
        main_ci.insert(0, ci("d02aa51", st, &[]));
    }
    let p41 = if step >= 2 {
        CiState::Passed
    } else {
        CiState::Running
    };
    let p42 = if step >= 3 {
        CiState::Failed
    } else {
        CiState::Running
    };
    let mut pulls = vec![
        ("pr1aaaa", 41, "feat/export", "Export to CSV"),
        ("pr2bbbb", 42, "fix/timezone", "Fix timezone drift"),
        ("pr3cccc", 43, "feat/search", "Search across projects"),
    ];
    if step >= 1 {
        pulls.insert(0, ("pr4dddd", 44, "feat/dark-mode", "Add dark mode"));
    }
    let snap = Snapshot {
        repo: Repo {
            owner: "demo".into(),
            name: "app".into(),
        },
        branch: "main".into(),
        fetched_ms: now_ms,
        commits: main_ci,
        pr_ci: vec![
            ci("pr1aaaa", p41, &[]),
            ci("pr2bbbb", p42, &["unit tests"]),
            ci("pr3cccc", CiState::Failed, &["unit tests"]),
        ],
        pulls: pulls
            .into_iter()
            .map(|(sha, n, br, t)| PullRequest {
                number: n,
                title: t.into(),
                draft: false,
                head_ref: br.into(),
                head_sha: format!("{sha}0000000000"),
                base_ref: "main".into(),
                author: "demo".into(),
                auto_merge: false,
            })
            .collect(),
        merge_queue: Some(vec![
            QueueEntry {
                position: 1,
                state: "AWAITING_CHECKS".into(),
                number: 42,
                title: "Fix timezone drift".into(),
            },
            QueueEntry {
                position: 2,
                state: "QUEUED".into(),
                number: 41,
                title: "Export to CSV".into(),
            },
        ]),
    };
    let wt = [
        WorktreeInfo {
            name: "builder-search".into(),
            branch: "feat/search".into(),
            ahead: 4,
            behind: 0,
        },
        WorktreeInfo {
            name: "builder-tz".into(),
            branch: "fix/timezone".into(),
            ahead: 2,
            behind: 0,
        },
        WorktreeInfo {
            name: "builder-csv".into(),
            branch: "feat/export".into(),
            ahead: 1,
            behind: 0,
        },
    ];
    assemble(&commits, Some("main"), &wt, Some(&Ok(snap)), now_ms)
}

/// Something that changed between two reads of the board. The flow view turns each into a
/// parcel travelling through the schematic; with no change there is no motion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowEvent {
    /// A pull request or branch appeared, or gained commits: a builder handed work off.
    HandedOff { label: String, branch: String },
    /// A hand-off's CI changed.
    HandoffCi {
        label: String,
        branch: String,
        to: Option<CiState>,
    },
    /// New commits reached the head of main (newest first).
    Landed { hash: String, ci: Option<CiState> },
}

/// What is new in `next` compared with `prev`. An empty `prev` (nothing read yet) yields
/// nothing, so the first read never fakes activity.
pub fn diff(prev: &Board, next: &Board) -> Vec<FlowEvent> {
    let mut out = Vec::new();
    if prev.loaded_ms == 0 {
        return out;
    }
    for h in &next.handoffs {
        match prev.handoffs.iter().find(|p| p.label == h.label) {
            None => out.push(FlowEvent::HandedOff {
                label: h.label.clone(),
                branch: h.branch.clone(),
            }),
            Some(p) => {
                if h.ahead > p.ahead {
                    out.push(FlowEvent::HandedOff {
                        label: h.label.clone(),
                        branch: h.branch.clone(),
                    });
                }
                if h.ci != p.ci {
                    out.push(FlowEvent::HandoffCi {
                        label: h.label.clone(),
                        branch: h.branch.clone(),
                        to: h.ci,
                    });
                }
            }
        }
    }
    // Commits newer than the old head. If the old head is gone from the list (history was
    // rewritten, or the window moved a lot), say nothing rather than guess.
    if let Some(old) = prev.main.first() {
        if let Some(i) = next.main.iter().position(|m| m.hash == old.hash) {
            for m in next.main[..i].iter().rev() {
                out.push(FlowEvent::Landed {
                    hash: m.hash.clone(),
                    ci: m.ci,
                });
            }
        }
    }
    out
}

/// Read git and (optionally) GitHub for the project at `dir`.
pub fn load(dir: &Path, now_ms: i64, use_github: bool) -> Result<Board> {
    let st = git::status(dir)?;
    let base = git::default_branch(&st.root).or_else(|| st.branch.clone());
    let commits = match &base {
        Some(b) => git::log_rev(&st.root, b, MAIN_COMMITS)?,
        None => git::log(&st.root, MAIN_COMMITS)?,
    };
    let mut worktrees = Vec::new();
    for w in &st.worktrees {
        let Some(branch) = w.branch.clone() else {
            continue;
        };
        if w.path == st.root || Some(&branch) == base.as_ref() {
            continue;
        }
        let (ahead, behind) = base
            .as_deref()
            .and_then(|b| git::ahead_behind(&st.root, b, &branch).ok())
            .unwrap_or((0, 0));
        let name = w
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| branch.clone());
        worktrees.push(WorktreeInfo {
            name,
            branch,
            ahead,
            behind,
        });
    }
    let gh = if use_github {
        Some(match github::remote_of(&st.root) {
            None => Err(Unavailable::NoRemote),
            Some(repo) => github::fetch(&repo, base.as_deref().unwrap_or("main"), now_ms),
        })
    } else {
        None
    };
    Ok(assemble(
        &commits,
        base.as_deref(),
        &worktrees,
        gh.as_ref(),
        now_ms,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{CommitCi, PullRequest, QueueEntry, Repo};

    fn commit(h: &str) -> Commit {
        Commit {
            hash: h.into(),
            ts_ms: 1,
            subject: format!("s {h}"),
        }
    }
    fn ci(sha: &str, state: CiState) -> CommitCi {
        CommitCi {
            sha: sha.into(),
            state,
            passed: 0,
            failed: u32::from(state == CiState::Failed),
            running: 0,
            cancelled: 0,
            failed_names: if state == CiState::Failed {
                vec!["www".into()]
            } else {
                vec![]
            },
            runs: vec![],
            url: None,
        }
    }
    fn pr(n: u64, branch: &str, sha: &str) -> PullRequest {
        PullRequest {
            number: n,
            title: format!("pr {n}"),
            draft: false,
            head_ref: branch.into(),
            head_sha: sha.into(),
            base_ref: "main".into(),
            author: "a".into(),
            auto_merge: false,
        }
    }
    fn snap() -> Snapshot {
        Snapshot {
            repo: Repo {
                owner: "o".into(),
                name: "r".into(),
            },
            branch: "main".into(),
            fetched_ms: 5,
            commits: vec![
                ci("aaa1111111", CiState::Failed),
                ci("bbb2222222", CiState::Failed),
                ci("ccc3333333", CiState::Passed),
            ],
            pr_ci: vec![ci("pppp999", CiState::Running)],
            pulls: vec![pr(7, "feat-a", "pppp999"), pr(8, "feat-b", "zzzz")],
            merge_queue: None,
        }
    }
    fn commits() -> Vec<Commit> {
        vec![commit("aaa1111"), commit("bbb2222"), commit("ccc3333")]
    }

    #[test]
    fn without_github_nothing_has_a_verdict_and_it_says_so() {
        let b = assemble(&commits(), Some("main"), &[], None, 9);
        assert_eq!(b.link, CiLink::NotChecked);
        assert!(b.main.iter().all(|m| m.ci.is_none()));
        assert!(b.culprit.is_none() && b.handoffs.is_empty());
        assert_eq!(b.merge_queue, None);
    }

    #[test]
    fn unavailable_github_keeps_the_reason() {
        let r = Err(Unavailable::NotSignedIn);
        let b = assemble(&commits(), Some("main"), &[], Some(&r), 9);
        assert!(matches!(&b.link, CiLink::Unavailable(t) if t.contains("not signed in")));
        assert!(b.main.iter().all(|m| m.ci.is_none()));
    }

    #[test]
    fn live_snapshot_marks_commits_and_finds_the_culprit() {
        let r = Ok(snap());
        let b = assemble(&commits(), Some("main"), &[], Some(&r), 9);
        let states: Vec<_> = b.main.iter().map(|m| m.ci).collect();
        assert_eq!(
            states,
            [
                Some(CiState::Failed),
                Some(CiState::Failed),
                Some(CiState::Passed)
            ]
        );
        let c = b.culprit.unwrap();
        assert_eq!((c.index, c.certain), (1, true));
        assert_eq!(b.main[1].failed_names, ["www"]);
        assert_eq!(b.merge_queue, Some(false));
    }

    #[test]
    fn pull_requests_become_handoffs_with_their_own_ci() {
        let r = Ok(snap());
        let wt = [WorktreeInfo {
            name: "wt-a".into(),
            branch: "feat-a".into(),
            ahead: 3,
            behind: 0,
        }];
        let b = assemble(&commits(), Some("main"), &wt, Some(&r), 9);
        assert_eq!(b.handoffs.len(), 2);
        let a = &b.handoffs[0];
        assert_eq!(a.label, "#7");
        assert_eq!(a.ci, Some(CiState::Running));
        assert_eq!((a.ahead, a.worktree.as_deref()), (Some(3), Some("wt-a")));
        assert_eq!(b.handoffs[1].ci, None); // no run found: not green
    }

    #[test]
    fn real_merge_queue_orders_handoffs_by_position() {
        let mut s = snap();
        s.merge_queue = Some(vec![
            QueueEntry {
                position: 1,
                state: "MERGEABLE".into(),
                number: 8,
                title: "pr 8".into(),
            },
            QueueEntry {
                position: 2,
                state: "AWAITING_CHECKS".into(),
                number: 99,
                title: "only in queue".into(),
            },
        ]);
        let r = Ok(s);
        let b = assemble(&commits(), Some("main"), &[], Some(&r), 9);
        let labels: Vec<_> = b.handoffs.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(labels, ["#8", "#99", "#7"]);
        assert_eq!(b.handoffs[0].queue, Some((1, "MERGEABLE".into())));
        assert_eq!(b.merge_queue, Some(true));
    }

    #[test]
    fn local_branch_ahead_without_a_pr_is_a_handoff_but_up_to_date_is_not() {
        let wt = [
            WorktreeInfo {
                name: "wt-x".into(),
                branch: "x".into(),
                ahead: 2,
                behind: 1,
            },
            WorktreeInfo {
                name: "wt-y".into(),
                branch: "y".into(),
                ahead: 0,
                behind: 4,
            },
        ];
        let b = assemble(&commits(), Some("main"), &wt, None, 9);
        assert_eq!(b.handoffs.len(), 1);
        assert_eq!(b.handoffs[0].pr, None);
        assert_eq!(b.handoffs[0].ahead, Some(2));
    }

    #[test]
    fn demo_board_shows_each_state_once() {
        let b = demo(1_000_000_000, 0);
        assert_eq!(b.main.len(), 8);
        assert_eq!(b.main[2].ci, None);
        let c = b.culprit.unwrap();
        // 3f8b21d is the oldest red commit, directly above the last green one.
        assert_eq!((c.index, c.certain), (3, true));
        assert_eq!(b.merge_queue, Some(true));
        assert_eq!(b.handoffs[0].label, "#42");
        assert_eq!(b.handoffs[2].ci, Some(CiState::Failed));
    }

    #[test]
    fn diff_reports_only_what_changed() {
        let a = demo(1_000, 0);
        // The first read has nothing to compare with.
        assert!(diff(&Board::empty(), &a).is_empty());
        assert!(diff(&a, &demo(2_000, 0)).is_empty());
        let e = diff(&a, &demo(2_000, 1));
        assert_eq!(
            e,
            [FlowEvent::HandedOff {
                label: "#44".into(),
                branch: "feat/dark-mode".into()
            }]
        );
        let e = diff(&demo(1, 3), &demo(2, 4));
        assert_eq!(
            e,
            [FlowEvent::Landed {
                hash: "d02aa51".into(),
                ci: Some(CiState::Running)
            }]
        );
        let e = diff(&demo(1, 2), &demo(2, 3));
        assert!(e.iter().any(|x| matches!(x, FlowEvent::HandoffCi { label, to: Some(CiState::Failed), .. } if label == "#42")));
        // The head vanishing from the new list is not reported as landings.
        let mut b = demo(2, 0);
        b.main.remove(0);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn load_reads_a_real_repo_with_a_worktree() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let o = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
        };
        run(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a"), "1").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "first"]);
        let wt = d.path().join("builder-1");
        run(
            &root,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );
        std::fs::write(wt.join("b"), "2").unwrap();
        run(&wt, &["add", "."]);
        run(&wt, &["commit", "-q", "-m", "second"]);

        let b = load(&root, 1, false).unwrap();
        assert_eq!(b.base.as_deref(), Some("main"));
        assert_eq!(b.main.len(), 1);
        assert_eq!(b.handoffs.len(), 1);
        let h = &b.handoffs[0];
        assert_eq!(h.branch, "feat");
        assert_eq!(h.ahead, Some(1));
        assert_eq!(h.worktree.as_deref(), Some("builder-1"));
        assert_eq!(b.link, CiLink::NotChecked);
    }
}
