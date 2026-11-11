//! The Pipeline tab: a quiet schematic of how work moves to main.
//!
//! builders (worktrees / Claude sessions) -> hand-off (merge queue or open pull requests)
//! -> integrator -> main track, with a return loop back to the builder when CI is red,
//! the Station test gate, the checks that ran on the newest commit, and what is waiting.
//!
//! Everything shown is read from git, GitHub (through `gh`) or recorded events. Parcels (the
//! small amber envelopes) only move when the board really changed between two reads: a branch
//! appeared, a pull request's CI changed, a commit reached main. Nothing loops for show.

use rune::tok::motion;
use rune::{Color, Cycle, El, LineCap, Path, Shape, Transition, col, el, oklch, row, text};
use rune_kit::Kit;
use station_core::board::{Board, CiLink, Handoff, MainDot};
use station_core::github::CiState;
use station_core::pipeline::{Gate, GateState, Pipeline};

pub const STAGE_W: f32 = 1000.0;
pub const STAGE_H: f32 = 584.0;
/// How long a parcel stays on screen.
pub const PARCEL_MS: i64 = 7_000;

const MAX_BUILDERS: usize = 4;
const MAX_DOTS: usize = 8;
const MAX_SLOTS: usize = 3;
const FRESH_MS: i64 = 20_000;

// Anchor points of the schematic.
const BUILD_X: f32 = 76.0;
const ROW_Y: [f32; 4] = [96.0, 138.0, 180.0, 222.0];
const HAND: [f32; 4] = [300.0, 70.0, 84.0, 196.0];
const HAND_C: [f32; 2] = [342.0, 168.0];
const INT: [f32; 4] = [540.0, 108.0, 120.0, 120.0];
const INT_C: [f32; 2] = [600.0, 168.0];
const MAIN_Y: f32 = 168.0;
const DOT_X0: f32 = 706.0;
const DOT_DX: f32 = 31.0;
const LOOP_Y: f32 = 340.0;
const LOOP_X: f32 = 12.0;

struct Pal {
    ink: Color,
    ink_soft: Color,
    ok: Color,
    fail: Color,
    run: Color,
    amber: Color,
}

fn pal() -> Pal {
    Pal {
        ink: oklch(0.42, 0.006, 270.0),
        ink_soft: oklch(0.30, 0.005, 270.0),
        ok: oklch(0.80, 0.15, 152.0),
        fail: oklch(0.68, 0.2, 22.0),
        run: oklch(0.78, 0.12, 235.0),
        amber: oklch(0.84, 0.12, 82.0),
    }
}

fn ci_color(p: &Pal, ci: Option<CiState>) -> Option<Color> {
    match ci {
        Some(CiState::Passed) => Some(p.ok),
        Some(CiState::Failed) => Some(p.fail),
        Some(CiState::Running) => Some(p.run),
        Some(CiState::Cancelled) | None => None,
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn ago(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{} min", s / 60),
        _ => format!("{} h", s / 3600),
    }
}

fn pulse(e: El, period: f32) -> El {
    e.cycle(Cycle {
        period,
        phase: 0.0,
        stops: &[(0.0, 1.0), (0.5, 0.35), (1.0, 1.0)],
        still: 1.0,
        fps: 30.0,
    })
}

// ---- parcels ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ParcelKind {
    /// A builder handed work to the hand-off.
    HandedOff,
    /// A hand-off's CI went green: it waits at the integrator.
    Green,
    /// A hand-off's CI went red: it goes back to its builder.
    Red,
    /// A commit reached main through the integrator.
    Landed { hash: String, ci: Option<CiState> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parcel {
    pub id: u64,
    pub t0: i64,
    pub kind: ParcelKind,
    /// The branch it belongs to, to find its builder row. Empty if unknown.
    pub branch: String,
    pub label: String,
}

/// Where a parcel is at `age` ms: the last waypoint whose time has passed. The runtime's
/// springs glide it between them.
fn at_age(points: &[(i64, [f32; 2])], age: i64) -> [f32; 2] {
    points
        .iter()
        .rev()
        .find(|(t, _)| *t <= age)
        .map(|(_, p)| *p)
        .unwrap_or(points[0].1)
}

fn slot_y(i: usize) -> f32 {
    HAND[1] + 24.0 + i as f32 * 52.0 + 22.0
}

fn parcel_pos(kind: &ParcelKind, row_y: f32, slot: f32, age: i64) -> [f32; 2] {
    let chip = [BUILD_X + 28.0, row_y];
    match kind {
        ParcelKind::HandedOff => at_age(&[(0, chip), (450, [HAND_C[0], slot])], age),
        ParcelKind::Green => at_age(
            &[
                (0, [HAND_C[0], slot]),
                (500, [460.0, MAIN_Y]),
                (1700, INT_C),
            ],
            age,
        ),
        ParcelKind::Red => at_age(
            &[
                (0, [HAND_C[0], slot]),
                (500, [460.0, MAIN_Y]),
                (1500, [INT_C[0], INT[1] + INT[3] + 14.0]),
                (2600, [INT_C[0] - 20.0, LOOP_Y]),
                (3700, [300.0, LOOP_Y]),
                (4900, [LOOP_X, LOOP_Y - 60.0]),
                (5800, [LOOP_X + 4.0, row_y]),
                (6300, chip),
            ],
            age,
        ),
        ParcelKind::Landed { .. } => at_age(
            &[
                (0, [HAND_C[0], MAIN_Y]),
                (500, [460.0, MAIN_Y]),
                (1500, INT_C),
                (2900, [INT[0] + INT[2] + 12.0, MAIN_Y]),
                (3500, [DOT_X0, MAIN_Y]),
            ],
            age,
        ),
    }
}

/// Hashes whose landing parcel has not reached the main track yet: their dot stays hidden so
/// the dot appears when the parcel arrives.
fn landing(parcels: &[Parcel], now: i64) -> Vec<&str> {
    parcels
        .iter()
        .filter_map(|p| match &p.kind {
            ParcelKind::Landed { hash, .. } if now - p.t0 < 3_500 => Some(hash.as_str()),
            _ => None,
        })
        .collect()
}

// ---- builders --------------------------------------------------------------------------

struct Builder {
    name: String,
    branch: String,
    active: bool,
}

/// Worktrees git can see, then Claude sessions that are not already one of them.
fn builders(p: &Pipeline, b: &Board, now: i64) -> Vec<Builder> {
    let mut out: Vec<Builder> = Vec::new();
    for w in &b.worktrees {
        let active = p
            .lanes
            .iter()
            .any(|l| l.name == w.name && !l.ended && now - l.last_ts_ms < FRESH_MS);
        out.push(Builder {
            name: w.name.clone(),
            branch: w.branch.clone(),
            active,
        });
    }
    for l in &p.lanes {
        if out.iter().any(|x| x.name == l.name) {
            continue;
        }
        out.push(Builder {
            name: l.name.clone(),
            branch: String::new(),
            active: !l.ended && now - l.last_ts_ms < FRESH_MS,
        });
    }
    out.truncate(MAX_BUILDERS);
    out
}

fn row_of(bs: &[Builder], branch: &str) -> usize {
    bs.iter()
        .position(|b| !branch.is_empty() && b.branch == branch)
        .unwrap_or(0)
        .min(ROW_Y.len() - 1)
}

// ---- drawing ---------------------------------------------------------------------------

fn label(k: &Kit, s: &str, c: Color, x: f32, y: f32) -> El {
    text(s.to_owned())
        .key(format!("lbl:{s}"))
        .font(k.ty.mono)
        .color(c)
        .absolute(x, y)
}

fn bar(x: f32, y: f32, h: f32, c: Color) -> El {
    el().size(5.0, h).radius(2.5).fill(c).absolute(x, y)
}

fn rounded(path: Path, from: [f32; 2], r: f32, via: &[[f32; 2]], to: [f32; 2]) -> Path {
    // Polyline with rounded corners: each `via` point is a corner.
    let mut pts = vec![from];
    pts.extend_from_slice(via);
    pts.push(to);
    let mut p = path.move_to(from);
    for i in 1..pts.len() - 1 {
        let (a, c, b) = (pts[i - 1], pts[i], pts[i + 1]);
        let toward = |from: [f32; 2], to: [f32; 2]| {
            let d = [to[0] - from[0], to[1] - from[1]];
            let len = (d[0] * d[0] + d[1] * d[1]).sqrt().max(0.001);
            let k = r.min(len / 2.0) / len;
            [from[0] + d[0] * k, from[1] + d[1] * k]
        };
        p = p.line_to(toward(c, a)).quad_to(c, toward(c, b));
    }
    p.line_to(to)
}

fn wires(bs: &[Builder], b: &Board, pal: &Pal) -> El {
    let mut s = Shape::new();
    let n = bs.len().max(1).min(ROW_Y.len());
    for &y in ROW_Y.iter().take(n) {
        let path = Path::new().move_to([BUILD_X + 40.0, y]).cubic_to(
            [190.0, y],
            [210.0, HAND_C[1]],
            [HAND[0], HAND_C[1]],
        );
        s = s.stroke_path(&path, 1.4, LineCap::Round, pal.ink_soft);
    }
    // hand-off -> integrator
    s = s.line(
        [HAND[0] + HAND[2], HAND_C[1]],
        [INT[0], INT_C[1]],
        1.4,
        LineCap::Round,
        pal.ink_soft,
    );
    // The main track: green when its newest commit passed, red when it failed.
    let head = b.main.first().and_then(|m| m.ci);
    let track = ci_color(pal, head).map(|c| c.alpha(0.7)).unwrap_or(pal.ink);
    s = s.line(
        [INT[0] + INT[2], MAIN_Y],
        [DOT_X0 + DOT_DX * (MAX_DOTS as f32 - 1.0) + 14.0, MAIN_Y],
        1.6,
        LineCap::Round,
        track,
    );
    // Down from main to the test gate.
    s = s.line(
        [DOT_X0, MAIN_Y],
        [DOT_X0, 262.0],
        1.2,
        LineCap::Round,
        pal.ink_soft,
    );
    s = s.line(
        [DOT_X0, 262.0],
        [800.0, 262.0],
        1.2,
        LineCap::Round,
        pal.ink_soft,
    );
    // The return loop: integrator -> along the bottom -> up the left edge -> the builders.
    let lp = rounded(
        Path::new(),
        [INT_C[0] - 20.0, INT[1] + INT[3]],
        16.0,
        &[
            [INT_C[0] - 20.0, LOOP_Y],
            [LOOP_X, LOOP_Y],
            [LOOP_X, ROW_Y[0]],
        ],
        [BUILD_X - 48.0, ROW_Y[0]],
    );
    s = s.stroke_path(&lp, 1.2, LineCap::Round, pal.ink_soft);
    el().key("wires")
        .size(STAGE_W, STAGE_H)
        .absolute(0.0, 0.0)
        .shape(s)
}

fn builder_row(i: usize, bd: Option<&Builder>, k: &Kit, pal: &Pal) -> El {
    let y = ROW_Y[i];
    let active = bd.is_some_and(|b| b.active);
    let robot_c = if active { pal.amber } else { pal.ink };
    let robot = el()
        .key(format!("robot:{i}"))
        .size(18.0, 16.0)
        .radius(5.0)
        .border(1.4, robot_c)
        .absolute(BUILD_X - 40.0, y - 8.0)
        .child(
            el().size(3.0, 3.0)
                .radius(1.5)
                .fill(robot_c)
                .absolute(4.0, 6.0),
        )
        .child(
            el().size(3.0, 3.0)
                .radius(1.5)
                .fill(robot_c)
                .absolute(10.0, 6.0),
        );
    let line = |w: f32, c: Color| el().size(w, 2.0).radius(1.0).fill(c);
    let lc = if active { pal.amber } else { pal.ink };
    let mut lines = col().gap(3.0).child(line(18.0, lc)).child(line(12.0, lc));
    if active {
        lines = pulse(lines, 0.9);
    }
    let chip = el()
        .key(format!("chip:{i}"))
        .size(40.0, 24.0)
        .radius(6.0)
        .border(1.4, if active { pal.amber } else { pal.ink })
        .absolute(BUILD_X, y - 12.0)
        .child(el().absolute(9.0, 6.0).child(lines));
    let mut r = el()
        .key(format!("builder:{i}"))
        .size(STAGE_W, STAGE_H)
        .absolute(0.0, 0.0)
        .child(robot)
        .child(chip);
    if let Some(b) = bd {
        r = r.child(
            text(clip(&b.name, 14))
                .font(k.ty.small)
                .color(k.c.text_faint)
                .absolute(BUILD_X + 2.0, y + 12.0),
        );
    }
    r
}

fn hand_off(b: &Board, k: &Kit, pal: &Pal) -> El {
    let mut e = el()
        .key("hand-off")
        .size(HAND[2], HAND[3])
        .radius(16.0)
        .border(1.4, pal.ink)
        .absolute(HAND[0], HAND[1])
        .child(bar(-3.0, HAND[3] / 2.0 - 14.0, 28.0, pal.ink))
        .child(bar(HAND[2] - 2.0, HAND[3] / 2.0 - 14.0, 28.0, pal.ink));
    for i in 0..MAX_SLOTS {
        let cy = 24.0 + i as f32 * 52.0 + 22.0;
        let h = b.handoffs.get(i);
        let vc = h.and_then(|h| ci_color(pal, h.ci));
        let mut slot = el()
            .key(format!(
                "slot:{i}:{}",
                h.map(|h| h.label.as_str()).unwrap_or("")
            ))
            .size(40.0, 40.0)
            .radius(20.0)
            .border(
                1.3,
                vc.unwrap_or(if h.is_some() { pal.ink } else { pal.ink_soft }),
            )
            .absolute(HAND[2] / 2.0 - 20.0, cy - 20.0)
            .transition(Transition::scale(0.5).combined(Transition::opacity()));
        if let Some(h) = h {
            if let Some(c) = vc {
                slot = slot.fill(c.alpha(0.12));
            }
            if h.ci == Some(CiState::Running) {
                slot = pulse(slot, 1.3);
            }
            slot = slot.child(
                text(clip(&h.label, 5))
                    .font(k.ty.small)
                    .color(k.c.text_base)
                    .absolute(7.0, 12.0),
            );
        }
        e = e.child(slot);
    }
    e
}

/// The branch the integrator is working on: the head of the merge queue, else the newest
/// hand-off whose CI is running.
fn current(b: &Board) -> Option<&Handoff> {
    b.handoffs
        .iter()
        .filter(|h| h.queue.is_some())
        .min_by_key(|h| h.queue.as_ref().map_or(u32::MAX, |q| q.0))
        .or_else(|| b.handoffs.iter().find(|h| h.ci == Some(CiState::Running)))
}

fn integrator(b: &Board, now: i64, k: &Kit, pal: &Pal) -> El {
    let mut e = el()
        .key("integrator")
        .size(INT[2], INT[3])
        .radius(16.0)
        .border(1.4, pal.ink)
        .absolute(INT[0], INT[1])
        .child(bar(-3.0, INT[3] / 2.0 - 14.0, 28.0, pal.ink))
        .child(bar(INT[2] - 2.0, INT[3] / 2.0 - 14.0, 28.0, pal.ink))
        .child(
            el().size(36.0, 4.0)
                .radius(2.0)
                .fill(pal.ink)
                .absolute(INT[2] / 2.0 - 18.0, -2.0),
        )
        .child(
            el().size(36.0, 4.0)
                .radius(2.0)
                .fill(pal.ink)
                .absolute(INT[2] / 2.0 - 18.0, INT[3] - 2.0),
        );
    if let Some(h) = current(b) {
        let c = ci_color(pal, h.ci).unwrap_or(pal.amber);
        e = e
            .child(
                el().key(format!("work:{}", h.label))
                    .size(26.0, 18.0)
                    .radius(4.0)
                    .fill(pal.amber)
                    .absolute(INT[2] / 2.0 - 13.0, INT[3] / 2.0 - 16.0)
                    .transition(Transition::scale(0.4).combined(Transition::opacity()))
                    .child(el().size(5.0, 5.0).radius(2.5).fill(c).absolute(20.0, -2.0)),
            )
            .child(
                text(clip(&h.label, 8))
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .absolute(INT[2] / 2.0 - 12.0, INT[3] / 2.0 + 8.0),
            );
    }
    let sub = match b.main.first() {
        Some(m) => format!(
            "newest commit {} ago · {} in view",
            ago(now - m.ts_ms),
            b.main.len().min(MAX_DOTS)
        ),
        None => "no commits yet".to_owned(),
    };
    e.child(
        label(
            k,
            "integrator · one branch at a time",
            k.c.text_base,
            -64.0,
            -40.0,
        )
        .key("int-title"),
    )
    .child(
        text(sub)
            .font(k.ty.small)
            .color(pal.amber)
            .absolute(-52.0, -22.0),
    )
}

fn main_track(b: &Board, parcels: &[Parcel], now: i64, k: &Kit, pal: &Pal) -> Vec<El> {
    let _ = (parcels, now);
    let dots: Vec<(usize, &MainDot)> = b.main.iter().enumerate().take(MAX_DOTS).collect();
    let head = b.main.first().and_then(|m| m.ci);
    let (word, wc) = match head {
        Some(CiState::Passed) => ("green", pal.ok),
        Some(CiState::Failed) => ("red", pal.fail),
        Some(CiState::Running) => ("running", pal.run),
        _ => ("no verdict", k.c.text_faint),
    };
    let mut out = vec![
        row()
            .key("main-title")
            .absolute(INT[0] + INT[2] + 8.0, MAIN_Y - 54.0)
            .gap(8.0)
            .child(text("main".to_owned()).font(k.ty.mono).color(k.c.text_base))
            .child(
                text(format!("· {word}"))
                    .font(k.ty.mono)
                    .color(wc)
                    .key(format!("main-word:{word}"))
                    .transition(Transition::blur_replace()),
            ),
        bar(
            DOT_X0 + DOT_DX * (MAX_DOTS as f32 - 1.0) + 24.0,
            MAIN_Y - 14.0,
            28.0,
            pal.ink,
        ),
    ];
    for (slot, (i, m)) in dots.iter().enumerate() {
        let x = DOT_X0 + slot as f32 * DOT_DX;
        let vc = ci_color(pal, m.ci);
        let size = 14.0;
        let mut d = el()
            .key(format!("dot:{}", m.hash))
            .size(size, size)
            .radius(size / 2.0)
            .absolute(x - size / 2.0, MAIN_Y - size / 2.0)
            .transition(
                Transition::offset(-24.0, 0.0)
                    .combined(Transition::scale(0.4))
                    .combined(Transition::opacity()),
            );
        d = match (vc, m.ci) {
            (Some(c), _) => d.fill(c),
            (None, Some(CiState::Cancelled)) => d.border(1.6, pal.ink),
            (None, _) => d.fill(k.c.text_muted.alpha(0.55)),
        };
        if m.ci == Some(CiState::Running) {
            d = pulse(d, 1.2);
        }
        if slot == 0 {
            d = d.border(2.0, Color::WHITE.alpha(0.85));
        }
        if b.culprit.as_ref().is_some_and(|c| c.index == *i) {
            d = d.child(
                text("culprit".to_owned())
                    .font(k.ty.small)
                    .color(pal.fail)
                    .absolute(-14.0, -26.0),
            );
        }
        out.push(d);
    }
    out
}

/// One finished test as the runner printed it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tick {
    Pass,
    Fail,
    Skip,
}

/// What the gate shows of a test run Station started: its streamed output, read as it comes.
pub struct TestView {
    pub running: bool,
    pub ticks: Vec<Tick>,
    pub last_line: String,
    /// The "Run tests" button, built by the app because pressing it changes app state.
    pub button: El,
}

/// Count finished tests in TAP output (`ok 3 - name`, `not ok 4 - name`). Indented lines are
/// sub-tests and are not counted. Runners that print something else give no ticks, and the
/// gate then shows only that output is arriving.
pub fn ticks_from<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<Tick> {
    let mut out = Vec::new();
    for l in lines {
        if let Some(rest) = l.strip_prefix("not ok ") {
            if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                out.push(Tick::Fail);
            }
        } else if let Some(rest) = l.strip_prefix("ok ")
            && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            let up = l.to_ascii_uppercase();
            out.push(if up.contains("# SKIP") || up.contains("# TODO") {
                Tick::Skip
            } else {
                Tick::Pass
            });
        }
    }
    out
}

fn test_gate(g: &Gate, t: &TestView, now: i64, k: &Kit, pal: &Pal) -> Vec<El> {
    let (word, c, knob) = if t.running {
        // A scanner sweeps the slider while the run is open: it shows work, not progress.
        let p = (now.rem_euclid(1500) as f32) / 1500.0;
        ("running", pal.run, 1.0 - (2.0 * p - 1.0).abs())
    } else {
        match g.state {
            GateState::Passed => ("passed", pal.ok, 1.0),
            GateState::Failed => ("failed", pal.fail, 1.0),
            GateState::Running => ("running", pal.run, 0.5),
            GateState::Unknown => ("no evidence", k.c.text_faint, 0.0),
        }
    };
    let running = t.running || g.state == GateState::Running;
    let x = 800.0;
    let y = 236.0;
    let w = 124.0;
    let mut out = vec![
        row()
            .key("gate-title")
            .absolute(x - 6.0, y - 46.0)
            .gap(8.0)
            .child(
                text("test gate".to_owned())
                    .font(k.ty.mono)
                    .color(k.c.text_base),
            )
            .child(
                text(format!("· {word}"))
                    .font(k.ty.mono)
                    .color(c)
                    .key(format!("gate-word:{word}"))
                    .transition(Transition::blur_replace()),
            ),
        text(clip(&g.detail, 34))
            .key("gate-detail")
            .font(k.ty.small)
            .color(k.c.text_faint)
            .absolute(x - 6.0, y - 26.0),
        el().key("gate-box")
            .size(w, 44.0)
            .radius(14.0)
            .border(1.4, if running { pal.run } else { pal.ink })
            .absolute(x, y)
            .child(bar(-3.0, 8.0, 28.0, pal.ink))
            .child(bar(w - 2.0, 8.0, 28.0, pal.ink))
            .child(
                el().size(w - 44.0, 2.0)
                    .radius(1.0)
                    .fill(pal.ink)
                    .absolute(22.0, 21.0),
            )
            .child(
                el().key("gate-knob")
                    .size(10.0, 10.0)
                    .radius(5.0)
                    .fill(if g.state == GateState::Unknown && !t.running {
                        pal.ink
                    } else {
                        c
                    })
                    .absolute(17.0 + (w - 44.0) * knob, 17.0)
                    .motion(motion::GLIDE),
            ),
    ];

    // Under the slider: the button, one line on what the run is doing, then one square per
    // finished test.
    out.push(t.button.clone().absolute(x, y + 52.0));
    let (passed, failed) = t.ticks.iter().fold((0, 0), |(p, f), t| match t {
        Tick::Pass => (p + 1, f),
        Tick::Fail => (p, f + 1),
        Tick::Skip => (p, f),
    });
    let status = if t.running {
        if t.last_line.trim().is_empty() {
            "starting…".to_owned()
        } else {
            clip(t.last_line.trim(), 34)
        }
    } else if !t.ticks.is_empty() {
        format!("{passed} ok · {failed} failed")
    } else {
        String::new()
    };
    if !status.is_empty() {
        out.push(
            text(status)
                .key("gate-status")
                .font(k.ty.small)
                .color(if failed > 0 && !t.running {
                    pal.fail
                } else {
                    k.c.text_faint
                })
                .absolute(x, y + 88.0),
        );
    }
    const PITCH: f32 = 10.0;
    const PER_ROW: usize = 20;
    const MAX: usize = 60;
    let gy = y + 110.0;
    let gx = x - 60.0;
    for (i, tk) in t.ticks.iter().take(MAX).enumerate() {
        let col = match tk {
            Tick::Pass => pal.ok,
            Tick::Fail => pal.fail,
            Tick::Skip => pal.ink,
        };
        out.push(
            el().key(format!("tick{i}"))
                .size(7.0, 7.0)
                .radius(2.0)
                .fill(col)
                .absolute(
                    gx + (i % PER_ROW) as f32 * PITCH,
                    gy + (i / PER_ROW) as f32 * PITCH,
                )
                .transition(Transition::opacity().combined(Transition::blur(3.0))),
        );
    }
    if t.ticks.len() > MAX {
        out.push(
            text(format!("+{}", t.ticks.len() - MAX))
                .font(k.ty.small)
                .color(k.c.text_faint)
                .absolute(gx + PER_ROW as f32 * PITCH + 4.0, gy + 2.0 * PITCH - 4.0),
        );
    }
    if t.running && t.ticks.len() < MAX {
        // The square the next finished test will fill.
        let i = t.ticks.len();
        out.push(pulse(
            el().key("tick-next")
                .size(7.0, 7.0)
                .radius(2.0)
                .border(1.2, pal.run)
                .absolute(
                    gx + (i % PER_ROW) as f32 * PITCH,
                    gy + (i / PER_ROW) as f32 * PITCH,
                ),
            0.8,
        ));
    }
    out
}

fn chip(key: String, s: &str, state: Option<CiState>, k: &Kit, pal: &Pal) -> El {
    let c = ci_color(pal, state);
    let mut e = el()
        .key(key)
        .px(9.0)
        .py(3.0)
        .radius(6.0)
        .border(1.2, c.map(|c| c.alpha(0.7)).unwrap_or(pal.ink))
        .border_dash(4.0, 3.0)
        .transition(Transition::opacity().combined(Transition::blur(3.0)));
    if let Some(c) = c {
        e = e.fill(c.alpha(0.12));
    }
    if state == Some(CiState::Running) {
        e = pulse(e, 1.2);
    }
    e.child(text(s.to_owned()).font(k.ty.small).color(if c.is_some() {
        k.c.text_base
    } else {
        k.c.text_muted
    }))
}

fn rule(y: f32, pal: &Pal) -> El {
    el().key(format!("rule{y}"))
        .size(STAGE_W - 48.0, 1.0)
        .fill(pal.ink_soft)
        .absolute(24.0, y)
}

fn checks(b: &Board, k: &Kit, pal: &Pal) -> Vec<El> {
    let mut out = vec![rule(382.0, pal)];
    let head = b.main.first();
    let title = match head {
        Some(m) => format!("CHECKS · {}", m.hash),
        None => "CHECKS".to_owned(),
    };
    out.push(label(k, &title, k.c.text_faint, 24.0, 396.0));
    let runs = head.map(|m| m.runs.as_slice()).unwrap_or(&[]);
    let mut r = row().key("check-chips").gap(10.0).absolute(24.0, 424.0);
    if runs.is_empty() {
        let why = match &b.link {
            CiLink::Live { .. } => "GitHub reports no workflow runs for the newest commit",
            CiLink::Unavailable(w) => w.as_str(),
            CiLink::NotChecked => "GitHub is off: no workflow results to show",
        };
        out.push(
            text(why.to_owned())
                .key("checks-why")
                .font(k.ty.small)
                .color(k.c.text_muted)
                .absolute(24.0, 428.0),
        );
    } else {
        for run in runs.iter().take(8) {
            r = r.child(chip(
                format!("run:{}", run.name),
                &clip(&run.name, 18),
                Some(run.state),
                k,
                pal,
            ));
        }
        out.push(r);
        let red: Vec<&str> = runs
            .iter()
            .filter(|x| x.state == CiState::Failed)
            .map(|x| x.name.as_str())
            .collect();
        let note = if red.is_empty() {
            "workflows that ran on the newest commit, each with its own verdict".to_owned()
        } else {
            format!(
                "red: {} · the culprit is the oldest red commit after the last green",
                red.join(", ")
            )
        };
        out.push(
            text(clip(&note, 110))
                .key("waiting-note")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .absolute(24.0, 460.0),
        );
    }
    out
}

fn waiting(b: &Board, k: &Kit, pal: &Pal) -> Vec<El> {
    let mut out = vec![rule(488.0, pal)];
    let title = match b.merge_queue {
        Some(true) => "MERGE QUEUE · IN ORDER",
        Some(false) => "OPEN PULL REQUESTS · NO MERGE QUEUE ON THIS REPO",
        None => "BRANCHES AHEAD OF MAIN",
    };
    out.push(label(k, title, k.c.text_faint, 24.0, 500.0));
    if b.handoffs.is_empty() {
        out.push(
            text("nothing waiting".to_owned())
                .key("nothing-waiting")
                .font(k.ty.small)
                .color(k.c.text_muted)
                .absolute(24.0, 530.0),
        );
    }
    for (i, h) in b.handoffs.iter().take(2).enumerate() {
        let y = 526.0 + i as f32 * 28.0;
        let ci_word = match h.ci {
            Some(CiState::Passed) => "checks passed",
            Some(CiState::Failed) => "checks failed",
            Some(CiState::Running) => "checks running",
            Some(CiState::Cancelled) => "checks cancelled",
            None => "no checks found",
        };
        let q = match &h.queue {
            Some((p, s)) => format!("queue #{p} {}", s.to_lowercase().replace('_', " ")),
            None if h.draft => "draft".to_owned(),
            None => "not queued".to_owned(),
        };
        out.push(
            row()
                .key(format!("wait:{}", h.label))
                .absolute(24.0, y)
                .gap(14.0)
                .items_center()
                .child(
                    el().w(250.0).child(
                        text(clip(&format!("{}  {}", h.label, h.title), 30))
                            .font(k.ty.mono)
                            .color(k.c.text_muted),
                    ),
                )
                .child(chip(format!("w1:{}", h.label), "opened", None, k, pal))
                .child(chip(
                    format!("w2:{}:{ci_word}", h.label),
                    ci_word,
                    h.ci,
                    k,
                    pal,
                ))
                .child(chip(format!("w3:{}:{q}", h.label), &q, None, k, pal))
                .transition(Transition::offset(0.0, 10.0).combined(Transition::opacity())),
        );
    }
    out
}

fn slot_of(b: &Board, label: &str) -> usize {
    b.handoffs
        .iter()
        .take(MAX_SLOTS)
        .position(|h| h.label == label)
        .unwrap_or(0)
}

fn parcel_el(p: &Parcel, board: &Board, bs: &[Builder], now: i64, pal: &Pal, k: &Kit) -> El {
    let age = now - p.t0;
    let ry = ROW_Y[row_of(bs, &p.branch)];
    let slot = slot_y(slot_of(board, &p.label));
    let at = parcel_pos(&p.kind, ry, slot, age);
    let (c, dot) = match &p.kind {
        ParcelKind::Red => (pal.fail, pal.fail),
        ParcelKind::Green => (pal.amber, pal.ok),
        ParcelKind::Landed { ci, .. } => (pal.amber, ci_color(pal, *ci).unwrap_or(pal.ok)),
        ParcelKind::HandedOff => (pal.amber, pal.ok),
    };
    let mut e = el()
        .key(format!("parcel:{}", p.id))
        .size(24.0, 16.0)
        .radius(4.0)
        .fill(c)
        .absolute(at[0] - 12.0, at[1] - 8.0)
        .motion(motion::GLIDE)
        .transition(
            Transition::scale(0.3)
                .combined(Transition::opacity())
                .combined(Transition::blur(3.0)),
        )
        .child(
            el().size(6.0, 6.0)
                .radius(3.0)
                .fill(dot)
                .absolute(19.0, -3.0),
        );
    if p.kind == ParcelKind::Red && (2_000..6_000).contains(&age) {
        e = e.child(
            text("CI red · back to its builder".to_owned())
                .font(k.ty.small)
                .color(pal.fail)
                .absolute(-30.0, -22.0),
        );
    }
    e
}

#[allow(clippy::too_many_arguments)]
pub fn view(
    p: &Pipeline,
    board: &Board,
    parcels: &[Parcel],
    synthetic: bool,
    note: Option<&str>,
    now: i64,
    t: &TestView,
    viewport: (f32, f32),
    k: &Kit,
) -> El {
    let c = &k.c;
    let pal = pal();
    // Commits whose parcel is still on its way are held back, so each dot appears when its
    // parcel reaches the main track.
    let held = landing(parcels, now);
    let mut visible = board.clone();
    let removed = visible
        .main
        .iter()
        .take_while(|m| held.contains(&m.hash.as_str()))
        .count();
    visible.main.drain(..removed);
    if let Some(cu) = visible.culprit.as_mut() {
        if cu.index >= removed {
            cu.index -= removed;
        } else {
            visible.culprit = None;
        }
    }
    let board = &visible;
    let bs = builders(p, board, now);

    let mut head = col()
        .w_full()
        .gap(2.0)
        .px(4.0)
        .child(text("Pipeline".to_owned()).font(k.ty.body_medium).color(c.text_base))
        .child(
            text("How work reaches main, from git, GitHub and recorded events. Parcels move only when something really changed.".to_owned())
                .font(k.ty.small)
                .color(c.text_faint)
                .wrap(),
        );
    if synthetic {
        head = head.child(
            text("SYNTHETIC DATA. Not real activity.".to_owned())
                .font(k.ty.small_medium)
                .color(c.warning.fg),
        );
    }
    if let Some(n) = note {
        head = head.child(
            text(n.to_owned())
                .font(k.ty.small)
                .color(c.warning.fg)
                .wrap(),
        );
    }

    let (live_word, live_c, live_rest) = match &board.link {
        CiLink::Live {
            slug,
            branch,
            fetched_ms,
        } => (
            "LIVE",
            pal.ok,
            format!("{slug}@{branch} · read {} ago", ago(now - fetched_ms)),
        ),
        CiLink::Unavailable(why) => ("LOCAL", pal.amber, clip(why, 70)),
        CiLink::NotChecked => ("LOCAL", c.text_faint, "GitHub is off".to_owned()),
    };

    let mut stage = el()
        .key("stage")
        .size(STAGE_W, STAGE_H)
        .radius(18.0)
        .clip_rounded(18.0)
        .fill(oklch(0.125, 0.004, 270.0))
        .border(1.0, Color::WHITE.alpha(0.06))
        .motion(motion::MORPH)
        .child(
            row()
                .key("live")
                .absolute(24.0, 18.0)
                .gap(8.0)
                .child(text(live_word.to_owned()).font(k.ty.mono).color(live_c))
                .child(
                    text(format!("· {live_rest}"))
                        .font(k.ty.mono)
                        .color(c.text_faint),
                ),
        )
        .child(label(k, "builders", c.text_muted, BUILD_X - 40.0, 56.0))
        .child(wires(&bs, board, &pal))
        .child(label(
            k,
            "hand-off",
            c.text_base,
            HAND[0] - 4.0,
            HAND[1] - 30.0,
        ))
        .child(
            text(match board.merge_queue {
                Some(true) => "merge queue".to_owned(),
                Some(false) => "open PRs".to_owned(),
                None => "branches ahead".to_owned(),
            })
            .font(k.ty.mono)
            .color(c.text_faint)
            .absolute(HAND[0] + 84.0, HAND[1] - 30.0),
        );
    for i in 0..ROW_Y.len().min(bs.len().max(1)) {
        stage = stage.child(builder_row(i, bs.get(i), k, &pal));
    }
    if bs.is_empty() {
        stage = stage.child(
            text("no worktrees or sessions seen".to_owned())
                .key("no-builders")
                .font(k.ty.small)
                .color(c.text_faint)
                .absolute(BUILD_X + 48.0, ROW_Y[0] - 8.0),
        );
    }
    stage = stage
        .child(label(
            k,
            "own branch + worktree",
            c.text_faint,
            BUILD_X - 40.0,
            256.0,
        ))
        .child(hand_off(board, k, &pal))
        .child(integrator(board, now, k, &pal))
        .child(label(
            k,
            "back to the builder that wrote it",
            c.text_faint,
            120.0,
            LOOP_Y + 10.0,
        ));
    for e in main_track(board, parcels, now, k, &pal) {
        stage = stage.child(e);
    }
    for e in test_gate(&p.gate, t, now, k, &pal) {
        stage = stage.child(e);
    }
    for e in checks(board, k, &pal) {
        stage = stage.child(e);
    }
    for e in waiting(board, k, &pal) {
        stage = stage.child(e);
    }
    for pc in parcels.iter().filter(|x| now - x.t0 < PARCEL_MS) {
        stage = stage.child(parcel_el(pc, board, &bs, now, &pal, k));
    }

    // Draw the schematic as large as the window allows: the stage keeps its design size and
    // is scaled about its centre inside a holder of the scaled size.
    let z = zoom(viewport);
    let holder = el()
        .key("stage-holder")
        .size(STAGE_W * z, STAGE_H * z)
        .child(
            stage
                .scale(z)
                .absolute(STAGE_W * (z - 1.0) / 2.0, STAGE_H * (z - 1.0) / 2.0),
        );

    col()
        .key("flow")
        .w_full()
        .h_full()
        .padding(20.0)
        .gap(14.0)
        .child(head)
        .child(
            row()
                .w_full()
                .child(el().grow(1.0))
                .child(holder)
                .child(el().grow(1.0)),
        )
}

/// How much to enlarge the stage: as much as fits the pane (the size of the "flow" column) beside the heading and padding,
/// never smaller than 1 and never past 1.8.
pub fn zoom(viewport: (f32, f32)) -> f32 {
    let (w, h) = viewport;
    if w <= 0.0 || h <= 0.0 {
        return 1.0;
    }
    ((w - 40.0) / STAGE_W)
        .min((h - 115.0) / STAGE_H)
        .clamp(1.0, 1.8)
}

#[cfg(test)]
mod tests {
    #[test]
    fn zoom_fits_the_window_and_never_shrinks() {
        assert_eq!(zoom((0.0, 0.0)), 1.0);
        assert_eq!(zoom((800.0, 600.0)), 1.0);
        let z = zoom((1500.0, 1100.0));
        assert!((z - 1.46).abs() < 0.01, "{z}");
        assert_eq!(zoom((4000.0, 3000.0)), 1.8);
    }

    #[test]
    fn ticks_count_top_level_tap_results_only() {
        let out = "TAP version 13\n# Subtest: a\nok 1 - a\n    ok 1 - inner\nnot ok 2 - b\nok 3 - c # SKIP later\n# pass 1\n1..3";
        assert_eq!(
            ticks_from(out.lines()),
            vec![Tick::Pass, Tick::Fail, Tick::Skip]
        );
        assert!(ticks_from("collected 5 items\n.....".lines()).is_empty());
    }

    use super::*;

    #[test]
    fn a_parcel_walks_its_waypoints_in_order() {
        let k = ParcelKind::Landed {
            hash: "x".into(),
            ci: None,
        };
        let a = parcel_pos(&k, 96.0, 90.0, 0);
        let b = parcel_pos(&k, 96.0, 90.0, 1_600);
        let c = parcel_pos(&k, 96.0, 90.0, 9_000);
        assert_eq!(a, [HAND_C[0], MAIN_Y]);
        assert_eq!(b, INT_C);
        assert_eq!(c, [DOT_X0, MAIN_Y]);
    }

    #[test]
    fn a_red_parcel_returns_to_its_builder_row() {
        let end = parcel_pos(&ParcelKind::Red, 180.0, 90.0, 6_500);
        assert_eq!(end, [BUILD_X + 28.0, 180.0]);
    }

    #[test]
    fn landing_dots_stay_hidden_until_the_parcel_arrives() {
        let p = Parcel {
            id: 1,
            t0: 1_000,
            kind: ParcelKind::Landed {
                hash: "abc".into(),
                ci: None,
            },
            branch: String::new(),
            label: String::new(),
        };
        assert_eq!(landing(std::slice::from_ref(&p), 2_000), ["abc"]);
        assert!(landing(std::slice::from_ref(&p), 5_000).is_empty());
    }
}
