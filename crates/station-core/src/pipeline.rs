//! The data behind the animated Pipeline view. Everything here is derived
//! from recorded events, Station-run tests and `git log`. Nothing is
//! scripted: a lane only gets a packet when an event arrived, and the gate
//! only shows a verdict when a test result exists.

use crate::event::Event;
use crate::git::Commit;
use crate::store::TestRunRecord;
use crate::timeline::{timeline, Tone, RUNNING_WINDOW_MS};
use std::collections::HashMap;

/// Packets kept per lane. Older ones have scrolled off the track.
pub const LANE_PACKETS: usize = 18;
/// A lane with no event for this long is drawn idle.
pub const IDLE_AFTER_MS: i64 = 5 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketKind {
    Prompt,
    Edit,
    Command,
    /// A Bash command that looks like a test run (inferred).
    Test,
    Permission,
    Agent,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    /// Event id: stable, so the UI can tell which packet is new.
    pub key: String,
    pub ts_ms: i64,
    pub kind: PacketKind,
    pub tone: Tone,
    pub label: String,
    pub detail: String,
    pub inferred: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lane {
    pub session_id: String,
    /// Last folder name of the session's working directory.
    pub name: String,
    pub last_ts_ms: i64,
    /// A SessionEnd event was recorded.
    pub ended: bool,
    pub idle: bool,
    /// Oldest first, at most `LANE_PACKETS`.
    pub packets: Vec<Packet>,
}

/// What the test gate knows. `Unknown` is a real state, not a default pass.
#[derive(Debug, Clone, PartialEq)]
pub enum GateState {
    Unknown,
    Running,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GateSource {
    /// Station ran the command and saw its exit status.
    StationRun,
    /// Claude ran something that looks like a test; the outcome is only
    /// whether the tool call succeeded.
    ClaudeInferred,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gate {
    pub state: GateState,
    pub source: Option<GateSource>,
    pub ts_ms: Option<i64>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    /// Most recently active first.
    pub lanes: Vec<Lane>,
    pub gate: Gate,
    /// Newest first, as `git log` gives them.
    pub commits: Vec<Commit>,
}

fn kind_of(e: &Event) -> PacketKind {
    match e.event.as_str() {
        "UserPromptSubmit" => PacketKind::Prompt,
        "PermissionRequest" | "Notification" => PacketKind::Permission,
        "SubagentStart" | "SubagentStop" => PacketKind::Agent,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            if e.file.is_some() {
                PacketKind::Edit
            } else if e.cmd_kind.as_deref() == Some("test") {
                PacketKind::Test
            } else {
                PacketKind::Command
            }
        }
        _ => PacketKind::Other,
    }
}

fn folder(cwd: Option<&str>) -> String {
    cwd.and_then(|c| c.trim_end_matches('/').rsplit('/').next())
        .filter(|s| !s.is_empty())
        .unwrap_or("session")
        .to_string()
}

/// The gate from the newest piece of test evidence. A Station run wins over
/// an inferred Claude run only when it is newer.
pub fn gate(events: &[Event], runs: &[TestRunRecord], now_ms: i64) -> Gate {
    let station = runs.iter().max_by_key(|r| r.started_ms);
    let claude = events
        .iter()
        .filter(|e| e.cmd_kind.as_deref() == Some("test"))
        .filter(|e| matches!(e.event.as_str(), "PostToolUse" | "PostToolUseFailure"))
        .max_by_key(|e| e.ts_ms);
    let claude_running = events
        .iter()
        .filter(|e| e.cmd_kind.as_deref() == Some("test") && e.event == "PreToolUse")
        .filter(|e| now_ms - e.ts_ms <= RUNNING_WINDOW_MS)
        .filter(|e| {
            !events.iter().any(|o| {
                o.tool_use_id.is_some()
                    && o.tool_use_id == e.tool_use_id
                    && matches!(o.event.as_str(), "PostToolUse" | "PostToolUseFailure")
            })
        })
        .max_by_key(|e| e.ts_ms);

    let station_ts = station.map(|r| r.started_ms + r.duration_ms);
    let claude_ts = claude.map(|e| e.ts_ms);
    let use_station = match (station_ts, claude_ts) {
        (Some(s), Some(c)) => s >= c,
        (Some(_), None) => true,
        _ => false,
    };

    if let Some(run) = claude_running {
        if claude_ts.is_none_or(|c| run.ts_ms > c) && station_ts.is_none_or(|s| run.ts_ms > s) {
            return Gate {
                state: GateState::Running,
                source: Some(GateSource::ClaudeInferred),
                ts_ms: Some(run.ts_ms),
                detail: "a test-looking command is running".into(),
            };
        }
    }
    if use_station {
        let r = station.expect("checked");
        let (state, detail) = match r.exit_code {
            Some(0) => (GateState::Passed, format!("exit 0 · {}", r.command)),
            Some(n) => (GateState::Failed, format!("exit {n} · {}", r.command)),
            None => (
                GateState::Failed,
                format!("killed by a signal · {}", r.command),
            ),
        };
        return Gate {
            state,
            source: Some(GateSource::StationRun),
            ts_ms: station_ts,
            detail,
        };
    }
    if let Some(e) = claude {
        let ok = e.event == "PostToolUse";
        return Gate {
            state: if ok {
                GateState::Passed
            } else {
                GateState::Failed
            },
            source: Some(GateSource::ClaudeInferred),
            ts_ms: Some(e.ts_ms),
            detail: format!(
                "{} {} (command looked like a test)",
                e.cmd_head.as_deref().unwrap_or("command"),
                if ok { "succeeded" } else { "failed" }
            ),
        };
    }
    Gate {
        state: GateState::Unknown,
        source: None,
        ts_ms: None,
        detail: "no test result recorded".into(),
    }
}

/// `events` oldest first.
pub fn build(
    events: &[Event],
    runs: &[TestRunRecord],
    commits: Vec<Commit>,
    now_ms: i64,
) -> Pipeline {
    let by_id: HashMap<&str, &Event> = events.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut lanes: Vec<Lane> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    for row in timeline(events, now_ms) {
        let Some(sid) = row.session_id.clone() else {
            continue;
        };
        let Some(ev) = by_id.get(row.key.as_str()) else {
            continue;
        };
        let i = *index.entry(sid.clone()).or_insert_with(|| {
            lanes.push(Lane {
                session_id: sid.clone(),
                name: folder(ev.cwd.as_deref()),
                last_ts_ms: row.ts_ms,
                ended: false,
                idle: false,
                packets: Vec::new(),
            });
            lanes.len() - 1
        });
        let lane = &mut lanes[i];
        lane.last_ts_ms = lane.last_ts_ms.max(row.ts_ms);
        if ev.event == "SessionEnd" {
            lane.ended = true;
        }
        // Lifecycle rows only update the lane; they are not packets.
        if matches!(
            ev.event.as_str(),
            "SessionStart" | "SessionEnd" | "CwdChanged"
        ) {
            continue;
        }
        lane.packets.push(Packet {
            key: row.key,
            ts_ms: row.ts_ms,
            kind: kind_of(ev),
            tone: row.tone,
            label: row.label,
            detail: row.detail,
            inferred: row.inferred,
        });
        if lane.packets.len() > LANE_PACKETS {
            lane.packets.remove(0);
        }
    }
    for l in &mut lanes {
        l.idle = !l.ended && now_ms - l.last_ts_ms > IDLE_AFTER_MS;
    }
    lanes.sort_by_key(|l| std::cmp::Reverse(l.last_ts_ms));

    Pipeline {
        lanes,
        gate: gate(events, runs, now_ms),
        commits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, ts: i64, kind: &str, session: &str) -> Event {
        serde_json::from_value(serde_json::json!({
            "v":1,"id":id,"ts_ms":ts,"event":kind,"session_id":session,"cwd":"/work/app"
        }))
        .unwrap()
    }
    fn tool(id: &str, ts: i64, kind: &str, session: &str, use_id: &str) -> Event {
        let mut e = ev(id, ts, kind, session);
        e.tool = Some("Bash".into());
        e.tool_use_id = Some(use_id.into());
        e
    }
    fn run(code: Option<i32>, start: i64) -> TestRunRecord {
        TestRunRecord {
            id: 1,
            cwd: "/work/app".into(),
            command: "cargo test".into(),
            started_ms: start,
            duration_ms: 100,
            exit_code: code,
            output_tail: String::new(),
        }
    }

    #[test]
    fn lanes_group_by_session_newest_first_and_skip_lifecycle() {
        let evs = [
            ev("1", 100, "SessionStart", "a"),
            ev("2", 200, "UserPromptSubmit", "a"),
            ev("3", 300, "UserPromptSubmit", "b"),
        ];
        let p = build(&evs, &[], vec![], 400);
        assert_eq!(p.lanes.len(), 2);
        assert_eq!(p.lanes[0].session_id, "b");
        assert_eq!(p.lanes[1].packets.len(), 1); // SessionStart is not a packet
        assert_eq!(p.lanes[1].name, "app");
    }

    #[test]
    fn ended_and_idle_are_distinct() {
        let evs = [
            ev("1", 0, "UserPromptSubmit", "a"),
            ev("2", 0, "SessionEnd", "b"),
        ];
        let p = build(&evs, &[], vec![], IDLE_AFTER_MS + 1);
        let a = p.lanes.iter().find(|l| l.session_id == "a").unwrap();
        let b = p.lanes.iter().find(|l| l.session_id == "b").unwrap();
        assert!(a.idle && !a.ended); // no end event: we do not claim it ended
        assert!(b.ended && !b.idle);
    }

    #[test]
    fn lane_keeps_only_the_newest_packets() {
        let evs: Vec<Event> = (0..40)
            .map(|i| ev(&i.to_string(), i, "UserPromptSubmit", "a"))
            .collect();
        let p = build(&evs, &[], vec![], 100);
        assert_eq!(p.lanes[0].packets.len(), LANE_PACKETS);
        assert_eq!(p.lanes[0].packets.last().unwrap().key, "39");
    }

    #[test]
    fn gate_is_unknown_without_evidence() {
        let g = gate(&[ev("1", 1, "UserPromptSubmit", "a")], &[], 10);
        assert_eq!(g.state, GateState::Unknown);
        assert!(g.source.is_none());
    }

    #[test]
    fn station_run_sets_the_gate_from_the_exit_code() {
        assert_eq!(
            gate(&[], &[run(Some(0), 1000)], 5000).state,
            GateState::Passed
        );
        assert_eq!(
            gate(&[], &[run(Some(101), 1000)], 5000).state,
            GateState::Failed
        );
        assert_eq!(gate(&[], &[run(None, 1000)], 5000).state, GateState::Failed);
    }

    #[test]
    fn claude_test_command_is_inferred_and_labelled() {
        let mut post = tool("2", 500, "PostToolUseFailure", "a", "t");
        post.cmd_head = Some("pytest".into());
        post.cmd_kind = Some("test".into());
        let g = gate(&[post], &[], 1000);
        assert_eq!(g.state, GateState::Failed);
        assert_eq!(g.source, Some(GateSource::ClaudeInferred));
        assert!(g.detail.contains("looked like a test"));
    }

    #[test]
    fn newest_evidence_wins() {
        let mut post = tool("2", 500, "PostToolUse", "a", "t");
        post.cmd_kind = Some("test".into());
        // Station ran later and failed.
        let g = gate(&[post.clone()], &[run(Some(1), 2000)], 5000);
        assert_eq!(
            (g.state, g.source),
            (GateState::Failed, Some(GateSource::StationRun))
        );
        // Claude ran later and passed.
        let g = gate(&[post], &[run(Some(1), 0)], 5000);
        assert_eq!(
            (g.state, g.source),
            (GateState::Passed, Some(GateSource::ClaudeInferred))
        );
    }

    #[test]
    fn unfinished_test_command_is_running_only_inside_the_window() {
        let mut pre = tool("1", 1000, "PreToolUse", "a", "t");
        pre.cmd_kind = Some("test".into());
        assert_eq!(gate(&[pre.clone()], &[], 2000).state, GateState::Running);
        assert_eq!(
            gate(&[pre], &[], 1000 + RUNNING_WINDOW_MS + 1).state,
            GateState::Unknown
        );
    }

    #[test]
    fn kinds_are_classified() {
        let mut t = tool("1", 1, "PostToolUse", "a", "x");
        t.cmd_kind = Some("test".into());
        assert_eq!(kind_of(&t), PacketKind::Test);
        let mut f = tool("2", 1, "PostToolUse", "a", "y");
        f.file = Some("/work/app/a.rs".into());
        assert_eq!(kind_of(&f), PacketKind::Edit);
        assert_eq!(
            kind_of(&tool("3", 1, "PostToolUse", "a", "z")),
            PacketKind::Command
        );
        assert_eq!(
            kind_of(&ev("4", 1, "PermissionRequest", "a")),
            PacketKind::Permission
        );
    }
}
