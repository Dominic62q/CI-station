//! Station desktop UI (spike): sessions, live activity, git and test output,
//! all read from `station-core`. No simulated pass/fail: the only synthetic
//! data is the explicit `stress=N` mode, which is labelled on screen.
//!
//! Usage (positional args after `--`):
//!   station-ui project=/path/to/repo test="pytest -q" stress=5000
//! Headless: `station-ui --snap out.png --size 1180x720 -- project=/path`

mod flow_view;
mod pipeline_view;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rune::{
    AppSpec, Chrome, Cx, El, GrowthSpec, KeyEvent, Latest, ScrollSpec, Tx, Waker, col, row, text,
};
use rune_kit::shell::{self, PaneState, RowStatus, Sidebar, SidebarRow, SplitView, Toolbar};
use rune_kit::{Button, ButtonVariant, IconButton, IconButtonVariant, Kit, SegmentedControl};
use station_core::board;
use station_core::event::Event;
use station_core::git::{self, Attribution};
use station_core::github;
use station_core::pipeline::{self, Pipeline};
use station_core::spool;
use station_core::store::{SessionSummary, Store, TestRunRecord, now_ms};
use station_core::testrun;
use station_core::timeline::{TimelineRow, Tone, timeline};

const ROW: f32 = 45.0;
const EVENT_ROW: f32 = 28.0;
const OUT_ROW: f32 = 18.0;
const MAX_EVENTS: usize = 50_000;
/// The Pipeline only needs recent history, so its rebuild stays cheap.
const PIPELINE_EVENTS: usize = 2_000;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Activity,
    /// The schematic: builders, hand-off, integrator, main.
    Pipeline,
    /// The earlier glowing circuit.
    Circuit,
}

struct GitView {
    branch: String,
    head: String,
    worktrees: usize,
    /// (porcelain code, path, who changed it)
    changes: Vec<(String, String, String)>,
}

#[derive(Default)]
struct Shared {
    lines: Vec<String>,
    done: Option<TestRunRecord>,
    error: Option<String>,
}

struct OutLine {
    key: String,
    text: String,
}

struct TestState {
    shared: Arc<Mutex<Shared>>,
    lines: Vec<OutLine>,
    running: bool,
    last: Option<TestRunRecord>,
    error: Option<String>,
}

struct App {
    store: Store,
    stress: bool,
    project: Option<PathBuf>,
    test_cmd: Option<String>,
    /// `run=1`: start the test command as soon as the window is up.
    autorun: bool,
    sessions: Vec<SessionSummary>,
    selected: Option<String>,
    rows: Vec<TimelineRow>,
    total_events: usize,
    git: Result<GitView, String>,
    git_req: Latest<Result<git::GitStatus, String>>,
    log_req: Latest<Result<Vec<git::Commit>, String>>,
    board_req: Latest<Result<board::Board, String>>,
    /// Main track with CI, hand-offs and the culprit; empty until the first read lands.
    board: board::Board,
    /// Read GitHub through `gh` for the board (off with `github=off`).
    use_github: bool,
    /// `board=demo`: a made-up board, labelled synthetic on screen. Never read from GitHub.
    board_demo: bool,
    next_board: f64,
    /// Parcels travelling through the Pipeline schematic: one per real change in the board.
    parcels: Vec<flow_view::Parcel>,
    next_parcel: u64,
    /// Position in the synthetic story `board=demo` plays, and when it advances.
    demo_step: usize,
    next_demo: f64,
    tab: Tab,
    /// Which side panels were open before the Pipeline tab folded them away.
    panes_before: Option<(bool, bool)>,
    pipe: Pipeline,
    commits: Vec<git::Commit>,
    sidebar: PaneState,
    inspector: PaneState,
    ingest_note: String,
    next_ingest: f64,
    next_git: f64,
    waker: Option<Waker>,
    test: TestState,
}

fn arg(name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    rune::extra_args()
        .iter()
        .find_map(|a| a.strip_prefix(&prefix).map(str::to_owned))
}

fn stress_events(n: usize) -> Vec<Event> {
    let base = now_ms() - (n as i64) * 700;
    (0..n)
        .map(|i| {
            let session = format!("stress-session-{:02}", i % 24);
            let (kind, tool, file) = match i % 5 {
                0 => ("SessionStart", None, None),
                1 => (
                    "PostToolUse",
                    Some("Edit"),
                    Some(format!(
                        "/work/proj/src/module_{}/file_{}.rs",
                        i % 17,
                        i % 91
                    )),
                ),
                2 => ("PostToolUse", Some("Bash"), None),
                3 => ("PostToolUseFailure", Some("Bash"), None),
                _ => ("Stop", None, None),
            };
            Event {
                v: 1,
                id: format!("stress-{i}"),
                ts_ms: base + (i as i64) * 700,
                event: kind.into(),
                session_id: Some(session),
                cwd: Some("/work/proj".into()),
                agent_id: None,
                agent_type: None,
                transcript_path: None,
                tool: tool.map(Into::into),
                tool_use_id: Some(format!("tu-{i}")),
                file,
                cmd_head: (tool == Some("Bash")).then(|| "cargo".into()),
                cmd_kind: None,
                status: None,
                source: None,
                reason: None,
                notification_type: None,
            }
        })
        .collect()
}

fn init() -> App {
    let stress_n: usize = arg("stress").and_then(|v| v.parse().ok()).unwrap_or(0);
    let stress = stress_n > 0;
    let store = if stress {
        let s = Store::open_in_memory().expect("in-memory store");
        for e in stress_events(stress_n) {
            let _ = s.insert_event(&e, "stress", 0);
        }
        s
    } else {
        match Store::open(&station_core::db_path()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("station-ui: {e:#}");
                Store::open_in_memory().expect("in-memory store")
            }
        }
    };
    let mut app = App {
        store,
        stress,
        project: arg("project").map(PathBuf::from),
        test_cmd: arg("test"),
        autorun: arg("run").as_deref() == Some("1"),
        sessions: Vec::new(),
        selected: None,
        rows: Vec::new(),
        total_events: 0,
        git: Err("no project selected (pass project=/path/to/repo)".into()),
        git_req: Latest::default(),
        log_req: Latest::default(),
        board_req: Latest::default(),
        board: if arg("board").as_deref() == Some("demo") {
            board::demo(now_ms(), 0)
        } else {
            board::Board::empty()
        },
        board_demo: arg("board").as_deref() == Some("demo"),
        use_github: arg("github").as_deref() != Some("off"),
        next_board: 0.0,
        tab: match arg("tab").as_deref() {
            Some("pipeline") => Tab::Pipeline,
            Some("circuit") => Tab::Circuit,
            _ => Tab::Activity,
        },
        parcels: Vec::new(),
        next_parcel: 1,
        demo_step: 0,
        next_demo: 2.5,
        panes_before: None,
        pipe: pipeline::build(&[], &[], Vec::new(), 0),
        commits: Vec::new(),
        sidebar: PaneState::new(276.0).range(180.0, 520.0),
        inspector: PaneState::new(380.0).range(260.0, 640.0),
        ingest_note: String::new(),
        next_ingest: 0.0,
        next_git: 0.0,
        waker: None,
        test: TestState {
            shared: Arc::default(),
            lines: Vec::new(),
            running: false,
            last: None,
            error: None,
        },
    };
    app.refresh(true);
    if app.tab != Tab::Activity {
        let t = app.tab;
        app.set_tab(t);
    }
    // Blocking is fine once at startup, before the window is shown; later refreshes run on a worker.
    if let Some(p) = app.project.clone() {
        let res = git::status(&p).map_err(|e| format!("{e:#}"));
        app.apply_git(res);
    }
    app
}

impl App {
    /// Ingest new spool lines and rebuild what the views show. True when anything may have changed.
    fn refresh(&mut self, force: bool) -> bool {
        let mut changed = force;
        if !self.stress {
            match spool::ingest_dir(&self.store, &station_core::spool_dir()) {
                Ok(r) => {
                    if r.new > 0 {
                        changed = true;
                    }
                    self.ingest_note = if r.malformed > 0 || r.unsupported > 0 {
                        format!(
                            "{} malformed, {} newer-version lines skipped",
                            r.malformed, r.unsupported
                        )
                    } else {
                        String::new()
                    };
                }
                Err(e) => {
                    self.ingest_note = format!("spool unreadable: {e:#}");
                    changed = true;
                }
            }
        }
        if changed {
            self.sessions = self.store.sessions().unwrap_or_default();
            let mut events = self.store.recent_events(MAX_EVENTS).unwrap_or_default();
            self.total_events = events.len();
            self.rebuild_pipeline(&events);
            if let Some(sel) = &self.selected {
                events.retain(|e| e.session_id.as_deref() == Some(sel.as_str()));
            }
            self.rows = timeline(&events, now_ms());
        }
        changed
    }

    /// Switch tabs. The Pipeline stage is wide, so it folds the side panels away and brings back
    /// whichever were open when you leave.
    fn set_tab(&mut self, tab: Tab) {
        if tab != Tab::Activity && self.panes_before.is_none() {
            self.panes_before = Some((self.sidebar.is_open(), self.inspector.is_open()));
            if self.sidebar.is_open() {
                self.sidebar.toggle();
            }
            if self.inspector.is_open() {
                self.inspector.toggle();
            }
        } else if tab == Tab::Activity
            && let Some((side, insp)) = self.panes_before.take()
        {
            if side && !self.sidebar.is_open() {
                self.sidebar.toggle();
            }
            if insp && !self.inspector.is_open() {
                self.inspector.toggle();
            }
        }
        if tab != Tab::Activity && now_ms() - self.board.loaded_ms > 20_000 {
            self.next_board = 0.0; // stale: read again now instead of waiting for the minute
        }
        self.tab = tab;
    }

    /// Rebuild the Pipeline model from the newest events (all sessions, not just the selected one).
    /// Returns true when it differs from what is on screen.
    fn rebuild_pipeline(&mut self, events: &[Event]) -> bool {
        let recent = &events[events.len().saturating_sub(PIPELINE_EVENTS)..];
        let runs = self.store.recent_test_runs(20).unwrap_or_default();
        let next = pipeline::build(recent, &runs, self.commits.clone(), now_ms());
        let differs = next != self.pipe;
        self.pipe = next;
        differs
    }

    /// Ask a worker for fresh git state; it lands in `poll` through `git_req.take()`.
    fn request_git(&self) {
        if let Some(project) = self.project.clone() {
            self.log_req.request(project.clone(), |p| {
                git::log(&p, 14).map_err(|e| format!("{e:#}"))
            });
            self.git_req
                .request(project, |p| git::status(&p).map_err(|e| format!("{e:#}")));
        }
    }

    /// Take a freshly read board. What changed since the last one becomes parcels; the first
    /// read, with nothing to compare, adds none.
    fn apply_board(&mut self, next: board::Board) {
        let now = now_ms();
        for ev in board::diff(&self.board, &next) {
            let (kind, branch, label) = match ev {
                board::FlowEvent::HandedOff { label, branch } => {
                    (flow_view::ParcelKind::HandedOff, branch, label)
                }
                board::FlowEvent::HandoffCi { label, branch, to } => match to {
                    Some(github::CiState::Failed) => (flow_view::ParcelKind::Red, branch, label),
                    Some(github::CiState::Passed) => (flow_view::ParcelKind::Green, branch, label),
                    _ => continue,
                },
                board::FlowEvent::Landed { hash, ci } => (
                    flow_view::ParcelKind::Landed {
                        hash: hash.clone(),
                        ci,
                    },
                    String::new(),
                    hash,
                ),
            };
            self.parcels.push(flow_view::Parcel {
                id: self.next_parcel,
                t0: now,
                kind,
                branch,
                label,
            });
            self.next_parcel += 1;
        }
        self.board = next;
    }

    /// Read the board (git, then GitHub through `gh`) on a worker. Only the Pipeline tab shows it.
    fn request_board(&self) {
        if self.board_demo {
            return;
        }
        if let Some(project) = self.project.clone() {
            let gh = self.use_github;
            self.board_req.request(project, move |p| {
                board::load(&p, now_ms(), gh).map_err(|e| format!("{e:#}"))
            });
        }
    }

    /// Turn a git status into what the inspector shows, attributing each change from recorded
    /// edit events. Cheap (database lookups and file metadata), so it runs on the UI thread.
    fn apply_git(&mut self, res: Result<git::GitStatus, String>) {
        self.git = res.map(|st| {
            let mut changes = Vec::new();
            for c in &st.changes {
                let who = match git::attribute(&self.store, &st.root, c) {
                    Ok(Attribution::Claude { session_id }) => {
                        format!("claude {}", short(session_id.as_deref()))
                    }
                    Ok(Attribution::ClaudeThenModified { session_id }) => {
                        format!("claude {}, modified since", short(session_id.as_deref()))
                    }
                    Ok(Attribution::Unknown) => "source unknown".into(),
                    Err(e) => format!("error: {e:#}"),
                };
                changes.push((c.code.clone(), c.path.clone(), who));
            }
            GitView {
                branch: st.branch.unwrap_or_else(|| "(detached)".into()),
                head: st.head.unwrap_or_else(|| "(no commits)".into()),
                worktrees: st.worktrees.len(),
                changes,
            }
        });
    }

    fn start_tests(&mut self) {
        let (Some(cwd), Some(cmd)) = (self.project.clone(), self.test_cmd.clone()) else {
            return;
        };
        if self.test.running {
            return;
        }
        let shared = Arc::new(Mutex::new(Shared::default()));
        self.test = TestState {
            shared: Arc::clone(&shared),
            lines: Vec::new(),
            running: true,
            last: None,
            error: None,
        };
        let waker = self.waker.clone();
        let wake = move || {
            if let Some(w) = &waker {
                w();
            }
        };
        std::thread::spawn(move || {
            let res = testrun::run_command(&cwd, &cmd, |l| {
                if let Ok(mut g) = shared.lock() {
                    g.lines.push(l.to_string());
                }
                wake();
            });
            if let Ok(mut g) = shared.lock() {
                match res {
                    Ok(r) => g.done = Some(r),
                    Err(e) => g.error = Some(format!("{e:#}")),
                }
            }
            wake();
        });
    }

    /// Pull streamed output from the worker. True when something new arrived.
    fn drain_tests(&mut self) -> bool {
        let shared = Arc::clone(&self.test.shared);
        let Ok(mut g) = shared.lock() else {
            return false;
        };
        let mut changed = false;
        let seen = self.test.lines.len();
        if g.lines.len() > seen {
            for (i, l) in g.lines.iter().enumerate().skip(seen) {
                self.test.lines.push(OutLine {
                    key: format!("o{i}"),
                    text: l.clone(),
                });
            }
            changed = true;
        }
        if let Some(mut rec) = g.done.take() {
            rec.id = self.store.insert_test_run(&rec).unwrap_or(0);
            self.test.last = Some(rec);
            self.test.running = false;
            let events = self
                .store
                .recent_events(PIPELINE_EVENTS)
                .unwrap_or_default();
            self.rebuild_pipeline(&events);
            changed = true;
        }
        if let Some(e) = g.error.take() {
            self.test.error = Some(e);
            self.test.running = false;
            changed = true;
        }
        changed
    }
}

fn short(id: Option<&str>) -> String {
    id.map(|s| s.chars().take(8).collect())
        .unwrap_or_else(|| "?".into())
}

fn hms(ts_ms: i64) -> String {
    let s = ts_ms / 1000;
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

fn base_name(p: &str) -> &str {
    p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p)
}

fn tone_color(k: &Kit, t: Tone) -> rune::Color {
    match t {
        Tone::Ok => k.c.success.fg,
        Tone::Fail => k.c.danger.fg,
        Tone::Running => k.c.info.fg,
        Tone::Attention => k.c.warning.fg,
        Tone::Unknown => k.c.text_faint,
        Tone::Neutral => k.c.text_muted,
    }
}

fn poll(s: &mut App, clock: f64, tx: &mut Tx) -> bool {
    let mut changed = s.drain_tests();
    if let Some(res) = s.git_req.take() {
        s.apply_git(res);
        changed = true;
    }
    if let Some(res) = s.board_req.take() {
        // A failed read (not a git repo, say) leaves an empty board rather than a stale one.
        s.apply_board(res.unwrap_or_else(|_| board::Board::empty()));
        changed = true;
    }
    if let Some(res) = s.log_req.take() {
        s.commits = res.unwrap_or_default();
        let events = s.store.recent_events(PIPELINE_EVENTS).unwrap_or_default();
        s.rebuild_pipeline(&events);
        changed = true;
    }
    // Only the timer advances the schedule, so wakes caused by streamed test output or a
    // finished git request don't each add another wake of their own.
    if clock >= s.next_ingest {
        s.next_ingest = clock + 1.0;
        changed |= s.refresh(false);
        if !changed {
            // The clock moves "running" to "no result recorded" without any new event.
            let events = s.store.recent_events(PIPELINE_EVENTS).unwrap_or_default();
            changed |= s.rebuild_pipeline(&events);
        }
        if s.board_demo && s.tab != Tab::Activity && clock >= s.next_demo {
            // The made-up story: one step every couple of seconds, labelled synthetic on screen.
            if s.demo_step + 1 < board::DEMO_STEPS {
                s.demo_step += 1;
                let next = board::demo(now_ms(), s.demo_step);
                s.apply_board(next);
                changed = true;
            }
            s.next_demo = clock + 2.2;
        }
        if s.tab != Tab::Activity && clock >= s.next_board {
            s.next_board = clock + 30.0;
            s.request_board();
        }
        if clock >= s.next_git {
            s.next_git = clock + 5.0;
            s.request_git();
        }
        tx.wake_after(Duration::from_millis(1000));
    }
    // While a parcel is travelling, redraw often enough for its springs to follow it.
    let now = now_ms();
    s.parcels
        .retain(|p| now - p.t0 < flow_view::PARCEL_MS + 500);
    if (!s.parcels.is_empty() || s.test.running) && s.tab == Tab::Pipeline {
        changed = true;
        tx.wake_after(Duration::from_millis(120));
    }
    changed
}

fn setup(s: &mut App, waker: Waker) {
    s.git_req.set_waker(waker.clone());
    s.log_req.set_waker(waker.clone());
    s.board_req.set_waker(waker.clone());
    s.waker = Some(waker);
    if s.autorun {
        s.autorun = false;
        s.start_tests();
    }
}

fn event_row(r: &TimelineRow, k: &Kit) -> El {
    let c = &k.c;
    let mut detail = r.detail.clone();
    if r.inferred {
        detail.push_str("  · looks like a test run (inferred)");
    }
    row()
        .key(r.key.clone())
        .w_full()
        .px(16.0)
        .py(5.0)
        .gap(12.0)
        .center_y()
        .child(
            text(hms(r.ts_ms))
                .font(k.ty.mono)
                .color(c.text_faint)
                .w(64.0),
        )
        .child(
            text(r.label.clone())
                .font(k.ty.ui)
                .color(tone_color(k, r.tone))
                .w(168.0)
                .truncate(),
        )
        .child(
            text(detail)
                .font(k.ty.mono)
                .color(c.text_muted)
                .selectable(true)
                .grow(1.0)
                .truncate(),
        )
}

fn out_row(l: &OutLine, k: &Kit) -> El {
    row().key(l.key.clone()).w_full().px(12.0).py(1.0).child(
        text(l.text.clone())
            .font(k.ty.mono)
            .color(k.c.text_muted)
            .selectable(true)
            .grow(1.0)
            .truncate(),
    )
}

fn session_row(s: &App, i: usize, k: &Kit) -> El {
    let ses = &s.sessions[i];
    let id = ses.session_id.clone();
    let title = ses
        .cwd
        .as_deref()
        .map(base_name)
        .unwrap_or("(unknown project)")
        .to_owned();
    let state = if ses.ended {
        "ended".to_string()
    } else {
        "no end event".to_string()
    };
    let mut r = SidebarRow::new(title)
        .height(ROW)
        .subtitle(format!(
            "{} · {} events · {}",
            short(Some(&ses.session_id)),
            ses.event_count,
            state
        ))
        .selected(s.selected.as_deref() == Some(ses.session_id.as_str()))
        .on_select({
            let id = id.clone();
            move |s: &mut App, _| {
                s.selected = Some(id.clone());
                s.refresh(true);
            }
        });
    if !ses.ended && now_ms() - ses.last_ts_ms < 120_000 {
        r = r.status(RowStatus::Busy);
    }
    r.build(k).key(id)
}

fn sidebar(s: &App, k: &Kit) -> El {
    let kit = k.clone();
    Sidebar::new()
        .label("Sessions")
        .inset(0.0)
        .header_gap(4.0)
        .header_padding(0.0, 16.0)
        .header(
            SidebarRow::new("All sessions")
                .height(32.0)
                .detail(s.total_events.to_string())
                .selected(s.selected.is_none())
                .on_select(|s: &mut App, _| {
                    s.selected = None;
                    s.refresh(true);
                })
                .build(k)
                .key("all"),
        )
        .row_height(ROW)
        .virtual_rows(
            s.sessions.len(),
            |s: &App, i| Cow::Borrowed(s.sessions[i].session_id.as_str()),
            move |s: &App, i| session_row(s, i, &kit),
        )
        .build(k)
}

fn activity(s: &App, k: &Kit) -> El {
    let c = &k.c;
    let spool = station_core::spool_dir();
    let mut header = col()
        .w_full()
        .px(16.0)
        .py(12.0)
        .gap(2.0)
        .child(text("Activity").font(k.ty.body_medium).color(c.text_base));
    if s.stress {
        header = header.child(
            text("SYNTHETIC STRESS DATA (stress=N). Not real activity.")
                .font(k.ty.small_medium)
                .color(c.warning.fg),
        );
    } else if s.sessions.is_empty() {
        header = header.child(
            text(format!(
                "No events received yet from {}. Enable the Station plugin: claude --plugin-dir plugin/station",
                spool.display()
            ))
            .font(k.ty.small)
            .color(c.warning.fg)
            .wrap(),
        );
    } else {
        header = header.child(
            text(format!(
                "{} rows · source: {}",
                s.rows.len(),
                spool.display()
            ))
            .font(k.ty.small)
            .color(c.text_faint)
            .truncate(),
        );
    }
    if !s.ingest_note.is_empty() {
        header = header.child(
            text(s.ingest_note.clone())
                .font(k.ty.small)
                .color(c.danger.fg)
                .wrap(),
        );
    }
    let kit = k.clone();
    let rows = col()
        .key("rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(
            s.rows.len(),
            EVENT_ROW,
            |s: &App, i| Cow::Borrowed(s.rows[i].key.as_str()),
            move |s: &App, i| event_row(&s.rows[i], &kit),
        );
    let list = col()
        .key("activity")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .selection_group(true)
        .scroll(ScrollSpec { stick_bottom: true })
        .child(rows);
    col()
        .key("main")
        .w_full()
        .h_full()
        .child(header)
        .child(list)
}

fn inspector(s: &App, k: &Kit) -> El {
    let c = &k.c;
    let label = |t: &str| {
        text(t.to_owned())
            .font(k.ty.small_medium)
            .color(c.text_faint)
    };
    let mut g = col().w_full().gap(4.0).child(label("GIT"));
    match &s.git {
        Ok(v) => {
            g = g.child(
                text(format!(
                    "{} @ {}  ·  {} worktree(s)",
                    v.branch, v.head, v.worktrees
                ))
                .font(k.ty.mono)
                .color(c.text_base)
                .truncate(),
            );
            if v.changes.is_empty() {
                g = g.child(
                    text("clean".to_owned())
                        .font(k.ty.small)
                        .color(c.text_muted),
                );
            }
            for (code, path, who) in v.changes.iter().take(40) {
                let known = who.starts_with("claude");
                g = g.child(
                    row()
                        .key(format!("chg:{path}"))
                        .w_full()
                        .gap(8.0)
                        .child(
                            text(code.clone())
                                .font(k.ty.mono)
                                .color(c.text_faint)
                                .w(22.0),
                        )
                        .child(
                            text(path.clone())
                                .font(k.ty.mono)
                                .color(c.text_base)
                                .grow(1.0)
                                .truncate()
                                .selectable(true),
                        )
                        .child(text(who.clone()).font(k.ty.small).color(if known {
                            c.info.fg
                        } else {
                            c.text_faint
                        })),
                );
            }
        }
        Err(e) => g = g.child(text(e.clone()).font(k.ty.small).color(c.text_muted).wrap()),
    }

    let verdict: (String, rune::Color) = if s.test.running {
        ("running…".into(), c.info.fg)
    } else if let Some(e) = &s.test.error {
        (format!("could not start: {e}"), c.danger.fg)
    } else if let Some(r) = &s.test.last {
        let secs = r.duration_ms as f64 / 1000.0;
        match r.exit_code {
            Some(0) => (
                format!("PASSED (exit 0) in {secs:.1}s · ran by Station"),
                c.success.fg,
            ),
            Some(n) => (
                format!("FAILED (exit {n}) in {secs:.1}s · ran by Station"),
                c.danger.fg,
            ),
            None => (format!("KILLED by signal after {secs:.1}s"), c.danger.fg),
        }
    } else {
        ("no result yet".into(), c.text_faint)
    };
    let can_run = s.project.is_some() && s.test_cmd.is_some() && !s.test.running;
    let cmd_line = s
        .test_cmd
        .clone()
        .unwrap_or_else(|| "(pass test=\"pytest -q\")".into());
    let run_button = Button::new("Run tests")
        .variant(ButtonVariant::Contrast)
        .disabled(!can_run)
        .on_press(|s: &mut App| s.start_tests())
        .build(k)
        .key("run-tests");
    let kit = k.clone();
    let out_rows = col()
        .key("out-rows")
        .w_full()
        .growth(GrowthSpec::default())
        .virtual_rows(
            s.test.lines.len(),
            OUT_ROW,
            |s: &App, i| Cow::Borrowed(s.test.lines[i].key.as_str()),
            move |s: &App, i| out_row(&s.test.lines[i], &kit),
        );
    let out = col()
        .key("out")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .radius(k.radius.md)
        .fill(c.layer_01)
        .selection_group(true)
        .scroll(ScrollSpec { stick_bottom: true })
        .child(out_rows);
    let tests = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(6.0)
        .child(label("TESTS"))
        .child(
            text(cmd_line)
                .font(k.ty.mono)
                .color(c.text_muted)
                .truncate(),
        )
        .child(
            row().w_full().gap(10.0).center_y().child(run_button).child(
                text(verdict.0)
                    .font(k.ty.small)
                    .color(verdict.1)
                    .wrap()
                    .grow(1.0),
            ),
        )
        .child(out);

    col()
        .key("details")
        .w_full()
        .h_full()
        .padding(16.0)
        .gap(18.0)
        .child(g)
        .child(tests)
}

fn view(s: &App, cx: &mut Cx<App>) -> El {
    let k = Kit::neutral(cx.system_dark());
    let title = match &s.selected {
        Some(id) => format!("Session {}", short(Some(id))),
        None => "All sessions".to_owned(),
    };
    let toolbar = Toolbar::new().title(title).trailing(
        IconButton::new(&shell::INSPECTOR)
            .variant(IconButtonVariant::GhostMuted)
            .expanded(s.inspector.is_open())
            .label(if s.inspector.is_open() {
                "Hide inspector"
            } else {
                "Show inspector"
            })
            .on_press(|s: &mut App| s.inspector.toggle())
            .build(&k)
            .key("inspector-toggle"),
    );
    let tabs = SegmentedControl::new(
        &["Activity", "Pipeline", "Circuit"],
        Some(match s.tab {
            Tab::Activity => 0,
            Tab::Pipeline => 1,
            Tab::Circuit => 2,
        }),
    )
    .label("View")
    .width(Some(280.0))
    .on_pick(|s: &mut App, i, _| {
        s.set_tab(match i {
            1 => Tab::Pipeline,
            2 => Tab::Circuit,
            _ => Tab::Activity,
        })
    })
    .build(&k)
    .key("tabs");
    let toolbar = toolbar.center(tabs);
    let main = match s.tab {
        Tab::Activity => activity(s, &k),
        Tab::Pipeline => flow_view::view(
            &s.pipe,
            &s.board,
            &s.parcels,
            s.stress || s.board_demo,
            (!s.ingest_note.is_empty()).then_some(s.ingest_note.as_str()),
            now_ms(),
            &flow_view::TestView {
                running: s.test.running,
                ticks: flow_view::ticks_from(s.test.lines.iter().map(|l| l.text.as_str())),
                last_line: s
                    .test
                    .lines
                    .last()
                    .map(|l| l.text.clone())
                    .unwrap_or_default(),
                button: Button::new(if s.test.running {
                    "Running…"
                } else {
                    "Run tests"
                })
                .variant(ButtonVariant::Contrast)
                .disabled(!(s.project.is_some() && s.test_cmd.is_some() && !s.test.running))
                .on_press(|s: &mut App| s.start_tests())
                .build(&k)
                .key("run-tests-flow"),
            },
            cx.layout_size("flow").map_or((0.0, 0.0), |z| (z[0], z[1])),
            &k,
        ),
        Tab::Circuit => pipeline_view::view(
            &s.pipe,
            &s.board,
            s.stress || s.board_demo,
            (!s.ingest_note.is_empty()).then_some(s.ingest_note.as_str()),
            now_ms(),
            &k,
        ),
    };
    SplitView::new(main)
        .sidebar(&s.sidebar, |s: &mut App| &mut s.sidebar, sidebar(s, &k))
        .inspector(
            &s.inspector,
            |s: &mut App| &mut s.inspector,
            inspector(s, &k),
        )
        .toolbar(toolbar)
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(8.0)
        .toolbar_in_panel()
        .sidebar_padding(18.0, 10.0)
        .reduced_motion(cx.is_reduced_motion())
        .build(&k)
}

fn on_key(s: &mut App, ev: &KeyEvent, _: &mut Tx) -> bool {
    if shell::is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        return true;
    }
    if ev.is_command('i') && ev.mods.alt {
        s.inspector.toggle();
        return true;
    }
    false
}

fn main() {
    let chrome = Chrome {
        full_size_content: true,
        traffic_lights: Some((16.0, 25.0)),
        drag_regions: true,
    };
    rune::App::new(AppSpec {
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        min_size: Some((720.0, 420.0)),
        fonts: rune_theme_opencode::FONTS,
        families: rune_theme_opencode::FAMILIES,
        ..AppSpec::new("Station", (1440.0, 900.0), init, view)
    })
    .chrome(chrome)
    .run();
}
