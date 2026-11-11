//! Reads the hook spool files into the store.
//!
//! Safe to run any number of times: per-file byte offsets make it resumable,
//! and event ids make re-delivery harmless. A trailing line without a newline
//! is left for the next run, because the hook may still be writing it.

use crate::event::{Event, SUPPORTED_SCHEMA};
use crate::store::{now_ms, Store};
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct IngestReport {
    pub files: usize,
    pub new: usize,
    pub duplicates: usize,
    /// Lines that were not valid events. Counted, never silently dropped.
    pub malformed: usize,
    /// Lines from a newer spool schema than this build understands.
    pub unsupported: usize,
}

pub fn ingest_dir(store: &Store, spool_dir: &Path) -> Result<IngestReport> {
    let mut report = IngestReport::default();
    if !spool_dir.exists() {
        return Ok(report);
    }
    let mut files: Vec<_> = std::fs::read_dir(spool_dir)
        .with_context(|| format!("reading {}", spool_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    for f in files {
        ingest_file(store, &f, &mut report)?;
        report.files += 1;
    }
    Ok(report)
}

fn ingest_file(store: &Store, path: &Path, report: &mut IngestReport) -> Result<()> {
    let key = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut offset = store.get_offset(&key)?;
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    if len < offset {
        offset = 0; // file was truncated or replaced: start over, ids dedupe
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else {
        return Ok(()); // no complete line yet
    };
    let complete = &buf[..=last_nl];

    store.begin()?;
    let result = (|| -> Result<()> {
        let now = now_ms();
        for line in complete.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            match serde_json::from_slice::<Event>(line) {
                Ok(e) if e.v > SUPPORTED_SCHEMA => report.unsupported += 1,
                Ok(e) => {
                    if store.insert_event(&e, "claude", now)? {
                        report.new += 1;
                    } else {
                        report.duplicates += 1;
                    }
                }
                Err(_) => report.malformed += 1,
            }
        }
        store.set_offset(&key, offset + complete.len() as u64)
    })();
    match result {
        Ok(()) => store.commit(),
        Err(e) => {
            store.rollback();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(id: &str, ts: i64) -> String {
        format!(r#"{{"v":1,"id":"{id}","ts_ms":{ts},"event":"Stop","session_id":"s"}}"#)
    }

    #[test]
    fn ingests_resumes_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let p = dir.path().join("claude-2026-01-01.jsonl");
        std::fs::write(&p, format!("{}\n{}\n", line("a", 1), line("b", 2))).unwrap();

        let r = ingest_dir(&store, dir.path()).unwrap();
        assert_eq!((r.new, r.duplicates), (2, 0));

        // Second run with nothing new reads nothing.
        let r = ingest_dir(&store, dir.path()).unwrap();
        assert_eq!((r.new, r.duplicates), (0, 0));

        // Appended line is picked up; a repeated id is a duplicate.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{}", line("c", 3)).unwrap();
        writeln!(f, "{}", line("a", 1)).unwrap();
        let r = ingest_dir(&store, dir.path()).unwrap();
        assert_eq!((r.new, r.duplicates), (1, 1));
        assert_eq!(store.recent_events(10).unwrap().len(), 3);
    }

    #[test]
    fn partial_last_line_waits_for_its_newline() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let p = dir.path().join("claude-x.jsonl");
        let full = line("a", 1);
        std::fs::write(&p, &full[..20]).unwrap();
        let r = ingest_dir(&store, dir.path()).unwrap();
        assert_eq!((r.new, r.malformed), (0, 0));

        std::fs::write(&p, format!("{full}\n")).unwrap();
        let r = ingest_dir(&store, dir.path()).unwrap();
        assert_eq!((r.new, r.malformed), (1, 0));
    }

    #[test]
    fn bad_and_future_lines_are_counted_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let future = r#"{"v":99,"id":"f","ts_ms":1,"event":"Stop"}"#;
        std::fs::write(
            dir.path().join("a.jsonl"),
            format!("not json\n{future}\n{}\n", line("ok", 1)),
        )
        .unwrap();
        let r = ingest_dir(&store, dir.path()).unwrap();
        assert_eq!((r.new, r.malformed, r.unsupported), (1, 1, 1));
    }

    #[test]
    fn missing_spool_dir_is_empty_not_an_error() {
        let store = Store::open_in_memory().unwrap();
        let r = ingest_dir(&store, Path::new("/nonexistent/station/spool")).unwrap();
        assert_eq!(r, IngestReport::default());
    }

    #[test]
    fn truncated_file_restarts_from_the_top() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let p = dir.path().join("a.jsonl");
        std::fs::write(&p, format!("{}\n{}\n", line("a", 1), line("b", 2))).unwrap();
        ingest_dir(&store, dir.path()).unwrap();
        std::fs::write(&p, format!("{}\n", line("c", 3))).unwrap();
        let r = ingest_dir(&store, dir.path()).unwrap();
        assert_eq!(r.new, 1);
    }
}
