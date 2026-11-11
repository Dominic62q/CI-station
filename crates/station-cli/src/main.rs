//! `station`: command-line front end for the core. Also the proving ground
//! for the logic the desktop UI will use.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use station_core::event::Event;
use station_core::git::{self, Attribution};
use station_core::store::{now_ms, Store};
use station_core::{db_path, spool, spool_dir, testrun};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "station",
    version,
    about = "Local development mission control (CLI)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read new hook events from the spool into the local database.
    Ingest,
    /// Show recent activity (ingests first).
    Tail {
        #[arg(short, default_value_t = 30)]
        n: usize,
    },
    /// List known Claude sessions (ingests first).
    Sessions,
    /// Git branch, worktrees and changed files, with who changed what.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Main track with CI results, hand-offs and the culprit (reads GitHub with gh).
    Board {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Do not call GitHub.
        #[arg(long)]
        no_github: bool,
    },
    /// Run a test command and record its real exit status.
    Test {
        /// Directory to run in.
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// The command, e.g. -- pytest -q
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Show recent recorded test runs.
    Runs {
        #[arg(short, default_value_t = 5)]
        n: usize,
    },
    /// Delete all collected history (database contents). Spool files are kept.
    Wipe {
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("station: {e:#}");
        std::process::exit(2);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let store = Store::open(&db_path())?;
    match cli.cmd {
        Cmd::Ingest => {
            let r = ingest(&store)?;
            println!(
                "ingested from {}: {} new, {} duplicate, {} malformed, {} unsupported-version",
                spool_dir().display(),
                r.new,
                r.duplicates,
                r.malformed,
                r.unsupported
            );
        }
        Cmd::Tail { n } => {
            ingest(&store)?;
            let events = store.recent_events(n)?;
            if events.is_empty() {
                println!("no events yet. Is the Station plugin enabled in Claude Code?");
            }
            for e in events {
                println!("{}", fmt_event(&e));
            }
        }
        Cmd::Sessions => {
            ingest(&store)?;
            let sessions = store.sessions()?;
            if sessions.is_empty() {
                println!("no sessions seen yet");
            }
            let now = now_ms();
            for s in sessions {
                let state = if s.ended {
                    "ended".to_string()
                } else {
                    format!(
                        "no end event; last seen {}s ago",
                        (now - s.last_ts_ms) / 1000
                    )
                };
                println!(
                    "{}  {:>4} events  {}  [{}]",
                    s.session_id.chars().take(8).collect::<String>(),
                    s.event_count,
                    s.cwd.as_deref().unwrap_or("?"),
                    state
                );
            }
        }
        Cmd::Status { path } => {
            ingest(&store)?;
            let st = git::status(&path)?;
            println!("repo      {}", st.root.display());
            println!(
                "branch    {}   head {}",
                st.branch.as_deref().unwrap_or("(detached)"),
                st.head.as_deref().unwrap_or("(no commits)")
            );
            for w in &st.worktrees {
                println!(
                    "worktree  {}  {}",
                    w.path.display(),
                    w.branch.as_deref().unwrap_or("(detached)")
                );
            }
            if st.changes.is_empty() {
                println!("clean");
            }
            for c in &st.changes {
                let who = match git::attribute(&store, &st.root, c)? {
                    Attribution::Claude { session_id } => {
                        format!("claude ({})", short(session_id.as_deref()))
                    }
                    Attribution::ClaudeThenModified { session_id } => {
                        format!("claude ({}), modified since", short(session_id.as_deref()))
                    }
                    Attribution::Unknown => "source unknown".to_string(),
                };
                println!("{}  {}  <- {}", c.code, c.path, who);
            }
        }
        Cmd::Test { cwd, command } => {
            let cmdline = build_command_line(&command);
            println!("$ {cmdline}   (in {})", cwd.display());
            let mut rec = testrun::run_command(&cwd, &cmdline, |l| println!("{l}"))?;
            rec.id = store.insert_test_run(&rec)?;
            let verdict = match rec.exit_code {
                Some(0) => "PASSED (exit 0)".to_string(),
                Some(c) => format!("FAILED (exit {c})"),
                None => "KILLED by signal".to_string(),
            };
            println!(
                "--\n{verdict} in {:.1}s  [run #{}]",
                rec.duration_ms as f64 / 1000.0,
                rec.id
            );
            if rec.exit_code != Some(0) {
                std::process::exit(1);
            }
        }
        Cmd::Board { path, no_github } => {
            use station_core::board::{self, CiLink};
            use station_core::github::CiState;
            let b = board::load(&path, station_core::store::now_ms(), !no_github)?;
            match &b.link {
                CiLink::NotChecked => println!("CI: not checked (--no-github)"),
                CiLink::Unavailable(t) => println!("CI: {t}"),
                CiLink::Live { slug, branch, .. } => println!("CI: {slug} on {branch}"),
            }
            let mark = |c: Option<CiState>| match c {
                Some(CiState::Passed) => "pass   ",
                Some(CiState::Failed) => "FAIL   ",
                Some(CiState::Running) => "running",
                Some(CiState::Cancelled) => "cancel ",
                None => "no run ",
            };
            println!("main track ({}):", b.base.as_deref().unwrap_or("?"));
            for (i, m) in b.main.iter().enumerate() {
                let flag = match &b.culprit {
                    Some(c) if c.index == i => "  <- culprit",
                    _ => "",
                };
                println!("  {} {}  {}{flag}", mark(m.ci), m.hash, m.subject);
            }
            if let Some(c) = &b.culprit {
                println!(
                    "culprit: {} ({})",
                    b.main[c.index].hash,
                    if c.certain {
                        "first red commit after the last green".to_string()
                    } else {
                        format!("{} commit(s) in between have no verdict", c.unchecked)
                    }
                );
            }
            match b.merge_queue {
                Some(true) => println!("merge queue: on"),
                Some(false) => {
                    println!("merge queue: none on this repo; listing open pull requests")
                }
                None => {}
            }
            for h in &b.handoffs {
                let q = h
                    .queue
                    .as_ref()
                    .map(|(p, s)| format!(" queue#{p} {s}"))
                    .unwrap_or_default();
                println!(
                    "  hand-off {} {} {}{q}{}",
                    mark(h.ci),
                    h.label,
                    h.title,
                    h.worktree
                        .as_ref()
                        .map(|w| format!("  [worktree {w}]"))
                        .unwrap_or_default()
                );
            }
        }
        Cmd::Runs { n } => {
            let runs = store.recent_test_runs(n)?;
            if runs.is_empty() {
                println!("no test runs recorded");
            }
            for r in runs {
                let v = match r.exit_code {
                    Some(0) => "pass".to_string(),
                    Some(c) => format!("fail({c})"),
                    None => "killed".to_string(),
                };
                println!(
                    "#{:<3} {:<8} {:>6.1}s  {}  (in {})",
                    r.id,
                    v,
                    r.duration_ms as f64 / 1000.0,
                    r.command,
                    r.cwd
                );
            }
        }
        Cmd::Wipe { yes } => {
            if !yes {
                bail!("this deletes all recorded events and test runs; re-run with --yes");
            }
            store.delete_all_history()?;
            println!("history deleted");
        }
    }
    Ok(())
}

fn ingest(store: &Store) -> Result<spool::IngestReport> {
    spool::ingest_dir(store, &spool_dir())
}

/// One argument is taken as a full shell command (`station test "pytest -q && ls"`).
/// Several arguments are individually shell-quoted so they reach the program as typed.
fn build_command_line(args: &[String]) -> String {
    if let [one] = args {
        return one.clone();
    }
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(a: &str) -> String {
    let safe = !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
    if safe {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

fn short(id: Option<&str>) -> String {
    id.map(|s| s.chars().take(8).collect())
        .unwrap_or_else(|| "?".into())
}

fn fmt_event(e: &Event) -> String {
    let secs = e.ts_ms / 1000;
    let hms = format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    );
    let mut detail = String::new();
    if let Some(t) = &e.tool {
        detail.push_str(t);
    }
    if let Some(f) = &e.file {
        detail.push_str(&format!(" {f}"));
    }
    if let Some(c) = &e.cmd_head {
        detail.push_str(&format!(" {c}"));
    }
    if e.cmd_kind.as_deref() == Some("test") {
        detail.push_str(" [looks like a test run, inferred]");
    }
    if let Some(s) = &e.status {
        detail.push_str(&format!(" -> {s}"));
    }
    format!(
        "{hms}Z  {}  {:<18} {}",
        short(e.session_id.as_deref()),
        e.event,
        detail.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_arg_is_a_full_command() {
        assert_eq!(
            build_command_line(&v(&["pytest -q && ls"])),
            "pytest -q && ls"
        );
    }

    #[test]
    fn multiple_args_are_quoted_individually() {
        assert_eq!(
            build_command_line(&v(&["python3", "-c", "print('ok')"])),
            r#"python3 -c 'print('\''ok'\'')'"#
        );
        assert_eq!(
            build_command_line(&v(&["pytest", "-k", "a b"])),
            "pytest -k 'a b'"
        );
    }
}
