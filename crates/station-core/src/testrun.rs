//! Runs a user-configured test command and records the real result.
//!
//! The command is exactly what the user typed, run through `sh -c` in the
//! chosen directory. Nothing here is simulated: the exit code is the
//! process's own exit status.

use crate::store::{now_ms, TestRunRecord};
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Output kept per run. Older output is dropped from the front.
const MAX_TAIL_BYTES: usize = 64 * 1024;

/// Run `command` in `cwd`, calling `on_line` for each output line (stdout and
/// stderr combined) as it arrives.
pub fn run_command(
    cwd: &Path,
    command: &str,
    mut on_line: impl FnMut(&str),
) -> Result<TestRunRecord> {
    anyhow::ensure!(cwd.is_dir(), "{} is not a directory", cwd.display());
    anyhow::ensure!(!command.trim().is_empty(), "test command is empty");

    let started_ms = now_ms();
    let t0 = Instant::now();
    // `2>&1` keeps stdout and stderr in the order the program wrote them.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 2>&1; {command}"))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting `{command}`"))?;

    let mut tail = String::new();
    let stdout = child.stdout.take().context("no stdout handle")?;
    for line in BufReader::new(stdout).split(b'\n') {
        let line = line?;
        let text = String::from_utf8_lossy(&line);
        on_line(&text);
        tail.push_str(&text);
        tail.push('\n');
        if tail.len() > MAX_TAIL_BYTES * 2 {
            let mut cut = tail.len() - MAX_TAIL_BYTES;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
    }
    let status = child.wait()?;
    Ok(TestRunRecord {
        id: 0,
        cwd: cwd.display().to_string(),
        command: command.to_string(),
        started_ms,
        duration_ms: t0.elapsed().as_millis() as i64,
        exit_code: status.code(), // None if killed by a signal
        output_tail: tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_exit_code_and_both_streams() {
        let d = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        let r = run_command(d.path(), "echo out; echo err 1>&2; exit 3", |l| {
            lines.push(l.to_string())
        })
        .unwrap();
        assert_eq!(r.exit_code, Some(3));
        assert_eq!(lines, ["out", "err"]);
        assert!(r.output_tail.contains("err"));
    }

    #[test]
    fn success_is_exit_zero() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            run_command(d.path(), "true", |_| {}).unwrap().exit_code,
            Some(0)
        );
    }

    #[test]
    fn missing_program_is_a_failure_not_a_pass() {
        let d = tempfile::tempdir().unwrap();
        let r = run_command(d.path(), "definitely-not-a-real-program-xyz", |_| {}).unwrap();
        assert_eq!(r.exit_code, Some(127));
    }

    #[test]
    fn signal_kill_has_no_exit_code() {
        let d = tempfile::tempdir().unwrap();
        let r = run_command(d.path(), "kill -9 $$", |_| {}).unwrap();
        assert_eq!(r.exit_code, None);
    }

    #[test]
    fn runs_in_the_given_directory_and_rejects_bad_input() {
        let d = tempfile::tempdir().unwrap();
        let r = run_command(d.path(), "pwd", |_| {}).unwrap();
        let want = d.path().canonicalize().unwrap();
        assert_eq!(
            Path::new(r.output_tail.trim()).canonicalize().unwrap(),
            want
        );
        assert!(run_command(Path::new("/nonexistent/xyz"), "true", |_| {}).is_err());
        assert!(run_command(d.path(), "  ", |_| {}).is_err());
    }

    #[test]
    fn long_output_is_trimmed_to_the_tail() {
        let d = tempfile::tempdir().unwrap();
        let r = run_command(d.path(), "seq 1 100000", |_| {}).unwrap();
        assert!(r.output_tail.len() <= MAX_TAIL_BYTES * 2);
        assert!(r.output_tail.trim_end().ends_with("100000"));
    }
}
