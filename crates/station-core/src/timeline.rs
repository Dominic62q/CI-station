//! Turns raw events into the rows a person reads. Pure functions, so the
//! rules are tested here and the UI only draws what it is given.

use crate::event::Event;
use std::collections::{HashMap, HashSet};

/// A tool start with no matching result younger than this is "running".
/// Older than this it is "no result recorded": the end event may have been
/// missed, denied, or Station may not have been running.
pub const RUNNING_WINDOW_MS: i64 = 120_000;

/// A permission prompt still counts as waiting for this long, if nothing later happened.
pub const PERMISSION_WAIT_MS: i64 = 30 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Ok,
    Fail,
    Running,
    /// Needs a person: a permission prompt or notification.
    Attention,
    /// We do not know the outcome.
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimelineRow {
    /// Unique per row; the event id.
    pub key: String,
    pub ts_ms: i64,
    pub session_id: Option<String>,
    pub label: String,
    pub detail: String,
    pub tone: Tone,
    /// True when part of the row is Station's inference, not a reported fact.
    pub inferred: bool,
}

fn rel(path: &str, cwd: Option<&str>) -> String {
    match cwd {
        Some(c) => path
            .strip_prefix(c)
            .map(|p| p.trim_start_matches('/'))
            .filter(|p| !p.is_empty())
            .unwrap_or(path)
            .to_string(),
        None => path.to_string(),
    }
}

/// `events` must be oldest first. `now_ms` decides running vs. no-result.
pub fn timeline(events: &[Event], now_ms: i64) -> Vec<TimelineRow> {
    let finished: HashSet<&str> = events
        .iter()
        .filter(|e| e.event == "PostToolUse" || e.event == "PostToolUseFailure")
        .filter_map(|e| e.tool_use_id.as_deref())
        .collect();

    // A turn that ended (Stop, StopFailure) or a session that ended cannot still be running
    // a tool it started earlier. A denied tool call leaves a start with no result.
    let mut turn_end: HashMap<&str, i64> = HashMap::new();
    // Newest event per session, ignoring notifications: a permission prompt is only still
    // "waiting" when nothing came after it.
    let mut latest: HashMap<&str, i64> = HashMap::new();
    for e in events {
        let Some(sid) = e.session_id.as_deref() else {
            continue;
        };
        if matches!(e.event.as_str(), "Stop" | "StopFailure" | "SessionEnd") {
            let t = turn_end.entry(sid).or_insert(e.ts_ms);
            *t = (*t).max(e.ts_ms);
        }
        if e.event != "Notification" {
            let t = latest.entry(sid).or_insert(e.ts_ms);
            *t = (*t).max(e.ts_ms);
        }
    }
    let ended_after = |e: &Event| {
        e.session_id
            .as_deref()
            .and_then(|s| turn_end.get(s))
            .is_some_and(|&t| t > e.ts_ms)
    };

    let mut out = Vec::with_capacity(events.len());
    for e in events {
        let tool = e.tool.as_deref().unwrap_or("tool");
        let target = e
            .file
            .as_deref()
            .map(|f| rel(f, e.cwd.as_deref()))
            .or_else(|| e.cmd_head.clone())
            .unwrap_or_default();
        let tool_detail = if target.is_empty() {
            tool.to_string()
        } else {
            format!("{tool}  {target}")
        };
        let inferred = e.cmd_kind.as_deref() == Some("test");

        let (label, detail, tone): (&str, String, Tone) = match e.event.as_str() {
            "SessionStart" => (
                "session started",
                e.source.clone().unwrap_or_default(),
                Tone::Neutral,
            ),
            "SessionEnd" => (
                "session ended",
                e.reason.clone().unwrap_or_default(),
                Tone::Neutral,
            ),
            "UserPromptSubmit" => ("prompt sent", String::new(), Tone::Neutral),
            "PreToolUse" => {
                if e.tool_use_id
                    .as_deref()
                    .is_some_and(|id| finished.contains(id))
                {
                    continue; // the result row says it all
                }
                if now_ms - e.ts_ms <= RUNNING_WINDOW_MS && !ended_after(e) {
                    ("running", tool_detail, Tone::Running)
                } else {
                    ("no result recorded", tool_detail, Tone::Unknown)
                }
            }
            "PostToolUse" => ("done", tool_detail, Tone::Ok),
            "PostToolUseFailure" => ("failed", tool_detail, Tone::Fail),
            "PermissionRequest" => {
                // The hook gives this event no tool id, so the answer is not visible to us.
                let newest = e
                    .session_id
                    .as_deref()
                    .and_then(|s| latest.get(s))
                    .is_none_or(|&t| t <= e.ts_ms);
                if newest && now_ms - e.ts_ms <= PERMISSION_WAIT_MS {
                    ("waiting for permission", tool_detail, Tone::Attention)
                } else {
                    (
                        "permission asked, answer not recorded",
                        tool_detail,
                        Tone::Unknown,
                    )
                }
            }
            "Notification" => (
                "notification",
                e.notification_type.clone().unwrap_or_default(),
                Tone::Attention,
            ),
            "Stop" => ("turn finished", String::new(), Tone::Neutral),
            "StopFailure" => ("turn failed", "API error".into(), Tone::Fail),
            "SubagentStart" => (
                "subagent started",
                e.agent_type.clone().unwrap_or_default(),
                Tone::Neutral,
            ),
            "SubagentStop" => (
                "subagent finished",
                e.agent_type.clone().unwrap_or_default(),
                Tone::Neutral,
            ),
            "CwdChanged" => (
                "directory changed",
                e.cwd.clone().unwrap_or_default(),
                Tone::Neutral,
            ),
            other => (other, String::new(), Tone::Neutral),
        };
        out.push(TimelineRow {
            key: e.id.clone(),
            ts_ms: e.ts_ms,
            session_id: e.session_id.clone(),
            label: label.to_string(),
            detail,
            tone,
            inferred,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, ts: i64, kind: &str, tool_use: Option<&str>) -> Event {
        serde_json::from_value(serde_json::json!({
            "v":1,"id":id,"ts_ms":ts,"event":kind,"session_id":"s","cwd":"/p",
            "tool":"Edit","tool_use_id":tool_use,"file":"/p/src/a.rs"
        }))
        .unwrap()
    }

    #[test]
    fn finished_tool_hides_its_start_and_shows_relative_path() {
        let evs = [
            ev("1", 1000, "PreToolUse", Some("t")),
            ev("2", 1100, "PostToolUse", Some("t")),
        ];
        let rows = timeline(&evs, 2000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tone, Tone::Ok);
        assert_eq!(rows[0].detail, "Edit  src/a.rs");
    }

    #[test]
    fn unmatched_start_is_running_then_unknown() {
        let evs = [ev("1", 1000, "PreToolUse", Some("t"))];
        assert_eq!(timeline(&evs, 1000 + 1000)[0].tone, Tone::Running);
        let later = 1000 + RUNNING_WINDOW_MS + 1;
        let r = &timeline(&evs, later)[0];
        assert_eq!(r.tone, Tone::Unknown);
        assert_eq!(r.label, "no result recorded");
    }

    #[test]
    fn failure_and_permission_get_their_tones() {
        let evs = [
            ev("1", 1, "PostToolUseFailure", Some("a")),
            ev("2", 2, "PermissionRequest", Some("b")),
        ];
        let rows = timeline(&evs, 10);
        assert_eq!((rows[0].tone, rows[1].tone), (Tone::Fail, Tone::Attention));
    }

    #[test]
    fn a_start_is_not_running_after_its_turn_ended() {
        // Seen in a real session: a denied tool call leaves a start with no result.
        let evs = [
            ev("1", 1000, "PreToolUse", Some("t")),
            ev("2", 2000, "Stop", None),
        ];
        let r = &timeline(&evs, 3000)[0];
        assert_eq!(
            (r.tone, r.label.as_str()),
            (Tone::Unknown, "no result recorded")
        );
    }

    #[test]
    fn permission_prompt_waits_only_while_it_is_the_latest_event() {
        let asked = ev("1", 1000, "PermissionRequest", None);
        assert_eq!(
            timeline(std::slice::from_ref(&asked), 2000)[0].tone,
            Tone::Attention
        );
        let later = [asked, ev("2", 1500, "PostToolUse", Some("x"))];
        let r = &timeline(&later, 2000)[0];
        assert_eq!(
            (r.tone, r.label.as_str()),
            (Tone::Unknown, "permission asked, answer not recorded")
        );
    }

    #[test]
    fn inferred_test_command_is_flagged() {
        let mut e = ev("1", 1, "PostToolUse", Some("a"));
        e.tool = Some("Bash".into());
        e.file = None;
        e.cmd_head = Some("pytest".into());
        e.cmd_kind = Some("test".into());
        let r = &timeline(&[e], 10)[0];
        assert!(r.inferred);
        assert_eq!(r.detail, "Bash  pytest");
    }

    #[test]
    fn unknown_event_names_are_kept_not_dropped() {
        let e = ev("1", 1, "SomethingNew", None);
        assert_eq!(timeline(&[e], 10)[0].label, "SomethingNew");
    }
}
