//! The animated Pipeline tab: a glowing circuit that reacts to real events.
//!
//! Everything drawn comes from `station_core::pipeline` (recorded events, test runs, `git log`).
//! Effects fire only when something real happened:
//!
//! * an event launches a glowing orb from its session node, with a ripple at the node and a
//!   comet trail while it is new; older orbs glide along the rail as newer ones arrive;
//! * a failed result also flashes the rail red;
//! * the test gate is a reactor core: its colour, glow and symbol follow the newest test
//!   evidence, the symbol draws itself on, and a shockwave leaves the core when the verdict
//!   changes; it reads UNKNOWN until evidence exists;
//! * a tool call that is still in flight pulses; a permission prompt pulses amber;
//! * a commit that just landed blooms on the main track.
//!
//! The only continuous motion is those pulses. Nothing is looped for show or simulated.

use rune::tok::motion;
use rune::{Beat, Easing, Path, Sequence, Shadow};
use rune::{Color, Cycle, El, LineCap, Shape, Transition, col, el, oklch, row, text};
use rune_kit::Kit;
use station_core::board::{Board, CiLink, Handoff, MainDot};
use station_core::github::CiState;
use station_core::pipeline::{
    Gate, GateSource, GateState, LANE_PACKETS, Lane, Packet, PacketKind, Pipeline,
};
use station_core::timeline::Tone;
use std::f32::consts::FRAC_PI_4;

/// The stage is a fixed canvas, centred in the pane. It is wide on purpose: the Pipeline tab
/// folds the side panels away so it has room.
pub const STAGE_W: f32 = 1000.0;
const STAGE_H: f32 = 540.0;
const CORE: [f32; 2] = [640.0, 185.0];
const DOT: f32 = 16.0;
const MAX_LANES: usize = 6;
const MAX_COMMITS: usize = 8;
/// Hand-off chips drawn; the header says how many are waiting in all.
const MAX_HANDOFFS: usize = 3;
/// An event this young still gets its launch flourish when the view is built.
const FRESH_MS: i64 = 6_000;

static RIPPLE: [Beat; 2] = [
    Beat::new(0.0).opacity(0.95).scale(0.5),
    Beat::new(0.85)
        .opacity(0.0)
        .scale(3.4)
        .easing(Easing::EASE_OUT),
];
static SHOCK: [Beat; 3] = [
    Beat::new(0.0).opacity(0.0).scale(0.6),
    Beat::new(0.08)
        .opacity(0.9)
        .scale(0.7)
        .easing(Easing::EASE_OUT),
    Beat::new(1.3)
        .opacity(0.0)
        .scale(3.0)
        .easing(Easing::EASE_OUT),
];
static FLASH: [Beat; 3] = [
    Beat::new(0.0).opacity(0.0),
    Beat::new(0.08).opacity(1.0).easing(Easing::EASE_OUT),
    Beat::new(0.9).opacity(0.0).easing(Easing::EASE_OUT),
];

/// The stage palette. Vivid on purpose; meaning is carried by colour *and* shape.
struct Pal {
    ok: Color,
    fail: Color,
    run: Color,
    ask: Color,
    dim: Color,
    faint: Color,
    accent: Color,
}

fn pal() -> Pal {
    Pal {
        ok: oklch(0.84, 0.19, 158.0),
        fail: oklch(0.70, 0.24, 18.0),
        run: oklch(0.78, 0.15, 240.0),
        ask: oklch(0.84, 0.16, 78.0),
        dim: oklch(0.72, 0.03, 270.0),
        faint: oklch(0.52, 0.02, 270.0),
        accent: oklch(0.72, 0.2, 295.0),
    }
}

fn tone(p: &Pal, t: Tone) -> Color {
    match t {
        Tone::Ok => p.ok,
        Tone::Fail => p.fail,
        Tone::Running => p.run,
        Tone::Attention => p.ask,
        Tone::Unknown => p.faint,
        Tone::Neutral => p.dim,
    }
}

fn glow(c: Color, blur: f32, a: f32) -> Shadow {
    Shadow {
        blur,
        color: c.alpha(a),
        ..Shadow::default()
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A ring that expands and fades once, where something just happened.
fn ripple(key: String, at: [f32; 2], size: f32, c: Color) -> El {
    el().key(key)
        .size(size, size)
        .absolute(at[0], at[1])
        .radius(size / 2.0)
        .border(1.5, c)
        .sequence(Sequence::new(&RIPPLE))
}

/// A glowing sphere: bright highlight, body colour, soft halo.
fn sphere(size: f32, c: Color) -> El {
    el().size(size, size)
        .radius(size / 2.0)
        .fill(c)
        .radial_gradient(
            [0.35, 0.3],
            size * 0.75,
            [
                (0.0, Color::WHITE.alpha(0.9)),
                (0.3, c),
                (1.0, c.mix(Color::BLACK, 0.45)),
            ],
        )
        .box_shadow(glow(c, size * 1.1, 0.7))
}

/// A cubic Bézier in stage coordinates.
#[derive(Clone, Copy)]
struct Curve([[f32; 2]; 4]);

impl Curve {
    fn at(&self, t: f32) -> [f32; 2] {
        let [p0, p1, p2, p3] = self.0;
        let u = 1.0 - t;
        let f = |i: usize| {
            u * u * u * p0[i]
                + 3.0 * u * u * t * p1[i]
                + 3.0 * u * t * t * p2[i]
                + t * t * t * p3[i]
        };
        [f(0), f(1)]
    }
    /// Unit direction of travel at `t`.
    fn dir(&self, t: f32) -> [f32; 2] {
        let (a, b) = (self.at((t - 0.01).max(0.0)), self.at((t + 0.01).min(1.0)));
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let n = (dx * dx + dy * dy).sqrt().max(1e-3);
        [dx / n, dy / n]
    }
    fn path(&self) -> Path {
        let [p0, p1, p2, p3] = self.0;
        Path::new().move_to(p0).cubic_to(p1, p2, p3)
    }
}

/// Where a lane's orbs ride: from its node, weaving up or down, into the reactor core.
/// Neighbouring lanes swing opposite ways so the paths braid instead of running in parallel.
fn lane_curve(i: usize, n: usize) -> Curve {
    let mid = (n as f32 - 1.0) / 2.0;
    let y0 =
        (CORE[1] + (i as f32 - mid) * 78.0 + if n == 1 { -90.0 } else { 0.0 }).clamp(56.0, 330.0);
    let x0 = 70.0 + (i % 2) as f32 * 140.0;
    let dir = if i.is_multiple_of(2) { 1.0 } else { -1.0 };
    let end = [CORE[0] - 50.0, CORE[1] + (i as f32 - mid) * 10.0];
    Curve([
        [x0, y0],
        [x0 + 230.0, y0 - dir * 130.0],
        [end[0] - 210.0, end[1] + dir * 120.0],
        end,
    ])
}

/// Slot `rank` (0 = newest) along a lane: near the node first, drifting toward the core.
fn slot_t(rank: usize) -> f32 {
    0.07 + rank as f32 * (0.86 / (LANE_PACKETS as f32 - 1.0))
}

/// The line the main track follows along the bottom of the stage.
fn main_curve() -> Curve {
    Curve([
        [250.0, 440.0],
        [450.0, 520.0],
        [700.0, 350.0],
        [950.0, 420.0],
    ])
}

/// The flow out of the core toward the newest commit.
fn flow_out() -> Curve {
    Curve([
        [CORE[0], CORE[1] + 62.0],
        [CORE[0], CORE[1] + 190.0],
        [900.0, 310.0],
        [950.0, 420.0],
    ])
}

fn orb(p: &Packet, curve: &Curve, rank: usize, node: [f32; 2], now: i64, pal: &Pal) -> El {
    let c = tone(pal, p.tone);
    let fresh = now - p.ts_ms < FRESH_MS;
    let t = slot_t(rank);
    let at = curve.at(t);
    let (x, y) = (at[0] - DOT / 2.0, at[1] - DOT / 2.0);
    let mut o = el()
        .key(p.key.clone())
        .size(DOT, DOT)
        .absolute(x, y)
        // Launched from the session node: small and blurred, it flies out to its slot and
        // grows. The oldest orbs are drawn into the reactor core: they shrink as they go.
        .transition(Transition::asymmetric(
            Transition::offset(node[0] - at[0], node[1] - at[1])
                .combined(Transition::scale(0.2))
                .combined(Transition::blur(5.0)),
            Transition::offset(CORE[0] - at[0], CORE[1] - at[1])
                .combined(Transition::scale(0.1))
                .combined(Transition::opacity()),
        ));
    o = match p.kind {
        PacketKind::Prompt => o
            .radius(DOT / 2.0)
            .border(2.0, pal.accent)
            .box_shadow(glow(pal.accent, 14.0, 0.6)),
        PacketKind::Edit => o
            .radius(5.0)
            .fill(c)
            .gradient(Color::WHITE.alpha(0.35), c.alpha(0.0))
            .box_shadow(glow(c, 16.0, 0.65)),
        PacketKind::Command => o
            .radius(DOT / 2.0)
            .fill(c)
            .radial_gradient(
                [0.35, 0.3],
                DOT * 0.75,
                [
                    (0.0, Color::WHITE.alpha(0.9)),
                    (0.3, c),
                    (1.0, c.mix(Color::BLACK, 0.45)),
                ],
            )
            .box_shadow(glow(c, DOT * 1.1, 0.7)),
        PacketKind::Test => {
            let d = o
                .radius(3.0)
                .fill(c)
                .rotate(FRAC_PI_4)
                .box_shadow(glow(c, 18.0, 0.75));
            if p.inferred {
                d.border(1.5, Color::WHITE).border_dash(3.0, 2.0)
            } else {
                d
            }
        }
        PacketKind::Permission => o
            .radius(DOT / 2.0)
            .fill(pal.ask)
            .box_shadow(glow(pal.ask, 20.0, 0.9)),
        PacketKind::Agent => o.radius(5.0).border(2.0, c).box_shadow(glow(c, 12.0, 0.5)),
        PacketKind::Other => o.radius(DOT / 2.0).border(1.5, pal.faint),
    };
    // A comet trail behind the freshest orbs, turned to follow the curve it is riding.
    if rank < 2 {
        let a = if rank == 0 { 0.65 } else { 0.32 };
        let d = curve.dir(t);
        let len = 40.0;
        let angle = d[1].atan2(d[0]);
        let cx = DOT / 2.0 - d[0] * (len / 2.0 + 4.0);
        let cy = DOT / 2.0 - d[1] * (len / 2.0 + 4.0);
        o = o.child(
            el().size(len, 3.0)
                .radius(1.5)
                .absolute(cx - len / 2.0, cy - 1.5)
                .rotate(angle)
                .linear_gradient(
                    std::f32::consts::FRAC_PI_2,
                    [(0.0, c.alpha(0.0)), (1.0, c.alpha(a))],
                ),
        );
    }
    if fresh {
        o = o.child(ripple(format!("rip:{}", p.key), [0.0, 0.0], DOT, c));
    }
    if p.tone == Tone::Running || p.kind == PacketKind::Permission {
        o = o.cycle(Cycle {
            period: 1.1,
            phase: 0.0,
            stops: &[(0.0, 1.0), (0.5, 0.35), (1.0, 1.0)],
            still: 1.0,
            fps: 30.0,
        });
    }
    o
}

/// The stage's ground: every curve, drawn as a soft wide glow under a thin bright line.
fn wires(lanes: &[&Lane], n: usize, pal: &Pal) -> El {
    let mut shape = Shape::new();
    for (i, l) in lanes.iter().enumerate() {
        let live = !l.ended && !l.idle;
        let c = if live { pal.accent } else { pal.faint };
        let path = lane_curve(i, n).path();
        shape = shape
            .stroke_path(&path, 9.0, LineCap::Round, c.alpha(0.07))
            .stroke_path(&path, 1.6, LineCap::Round, c.alpha(0.55));
    }
    let main = main_curve().path();
    shape = shape
        .stroke_path(
            &flow_out().path(),
            1.6,
            LineCap::Round,
            Color::WHITE.alpha(0.16),
        )
        .stroke_path(&main, 9.0, LineCap::Round, Color::WHITE.alpha(0.05))
        .stroke_path(&main, 1.6, LineCap::Round, Color::WHITE.alpha(0.3));
    el().key("wires")
        .size(STAGE_W, STAGE_H)
        .absolute(0.0, 0.0)
        .shape(shape)
}

/// Faint, fixed specks for depth. Placed by a fixed formula: decoration, not data, never animated.
fn stars() -> Vec<El> {
    (0..34)
        .map(|i| {
            let f = i as f32;
            let x = (f * 211.7 + 37.0) % STAGE_W;
            let y = (f * 97.3 + 11.0) % STAGE_H;
            let a = 0.05 + ((f * 13.0) % 7.0) / 70.0;
            el().key(format!("star{i}"))
                .size(2.0, 2.0)
                .radius(1.0)
                .fill(Color::WHITE.alpha(a))
                .absolute(x, y)
        })
        .collect()
}

/// Adds a lane's node, label, ripple and orbs to the stage.
fn lane_parts(stage: El, l: &Lane, i: usize, n: usize, now: i64, k: &Kit, pal: &Pal) -> El {
    let c = &k.c;
    let curve = lane_curve(i, n);
    let node = curve.0[0];
    let live = !l.ended && !l.idle;
    let node_c = if live { pal.accent } else { pal.faint };
    let state = if l.ended {
        "ended"
    } else if l.idle {
        "idle · no end event"
    } else {
        "active"
    };
    let in_flight = l.packets.iter().any(|p| p.tone == Tone::Running);
    let sid = &l.session_id;

    let mut stage = stage;
    let mut n_el = sphere(24.0, node_c)
        .key(format!("node:{sid}"))
        .absolute(node[0] - 12.0, node[1] - 12.0)
        .transition(Transition::scale(0.2).combined(Transition::blur(5.0)));
    if in_flight {
        n_el = n_el.cycle(Cycle {
            period: 1.4,
            phase: 0.0,
            stops: &[(0.0, 1.0), (0.5, 0.55), (1.0, 1.0)],
            still: 1.0,
            fps: 30.0,
        });
    }
    stage = stage.child(n_el).child(
        col()
            .key(format!("label:{sid}"))
            // On the side the curve leaves from last, so the label never sits on the orbs.
            .absolute(
                node[0] - 12.0,
                if i.is_multiple_of(2) {
                    node[1] + 20.0
                } else {
                    node[1] - 46.0
                },
            )
            .gap(0.0)
            .child(text(l.name.clone()).font(k.ty.small_medium).color(if live {
                c.text_base
            } else {
                c.text_muted
            }))
            .child(
                text(format!("{} · {}", short(sid), state))
                    .font(k.ty.small)
                    .color(c.text_faint),
            ),
    );
    if let Some(p) = l.packets.last().filter(|p| now - p.ts_ms < FRESH_MS) {
        // Each new event sends a ripple out of its node.
        stage = stage.child(ripple(
            format!("noderip:{}", p.key),
            [node[0] - 12.0, node[1] - 12.0],
            24.0,
            tone(pal, p.tone),
        ));
    }
    if let Some(p) = l
        .packets
        .last()
        .filter(|p| p.tone == Tone::Fail && now - p.ts_ms < FRESH_MS)
    {
        // A fresh failure surges red along the whole curve, once.
        let path = curve.path();
        let surge = Shape::new()
            .stroke_path(&path, 12.0, LineCap::Round, pal.fail.alpha(0.35))
            .stroke_path(&path, 3.0, LineCap::Round, pal.fail);
        stage = stage.child(
            el().key(format!("flash:{}", p.key))
                .size(STAGE_W, STAGE_H)
                .absolute(0.0, 0.0)
                .shape(surge)
                .sequence(Sequence::new(&FLASH)),
        );
    }
    for (rank, p) in l.packets.iter().rev().enumerate() {
        stage = stage.child(orb(p, &curve, rank, node, now, pal));
    }
    stage
}

/// The gate's symbol, drawn as strokes so it traces itself on when the verdict changes.
fn symbol(state: &GateState, c: Color) -> El {
    let shape = match state {
        GateState::Passed => {
            Shape::new().stroke(vec![[9.0, 21.0], [18.0, 30.0], [34.0, 11.0]], 4.5, c)
        }
        GateState::Failed => Shape::new()
            .stroke(vec![[11.0, 11.0], [32.0, 32.0]], 4.5, c)
            .stroke(vec![[32.0, 11.0], [11.0, 32.0]], 4.5, c),
        GateState::Running => {
            Shape::new().stroke(vec![[21.0, 7.0], [21.0, 22.0], [31.0, 29.0]], 4.0, c)
        }
        GateState::Unknown => {
            Shape::new().stroke(vec![[12.0, 21.0], [30.0, 21.0]], 4.0, c.alpha(0.6))
        }
    };
    el().key(format!("sym:{state:?}"))
        .size(42.0, 42.0)
        .absolute(39.0, 39.0)
        .shape(shape)
        .transition(Transition::draw())
}

fn reactor(g: &Gate, now: i64, k: &Kit, pal: &Pal) -> El {
    let c = &k.c;
    let (word, col_) = match g.state {
        GateState::Passed => ("PASSED", pal.ok),
        GateState::Failed => ("FAILED", pal.fail),
        GateState::Running => ("RUNNING", pal.run),
        GateState::Unknown => ("UNKNOWN", pal.faint),
    };
    let source = match g.source {
        Some(GateSource::StationRun) => "observed: Station ran it",
        Some(GateSource::ClaudeInferred) => "inferred from a Claude command",
        None => "no test evidence yet",
    };
    let fresh = g.ts_ms.is_some_and(|t| now - t < FRESH_MS) && g.state != GateState::Unknown;
    let mut core = el()
        .key("core")
        .size(120.0, 120.0)
        .absolute(CORE[0] - 60.0, CORE[1] - 60.0)
        .motion(motion::MORPH)
        .child(
            el().key("halo")
                .size(120.0, 120.0)
                .radius(60.0)
                .absolute(0.0, 0.0)
                .radial_gradient(
                    [0.5, 0.5],
                    60.0,
                    [
                        (0.0, col_.alpha(0.45)),
                        (0.55, col_.alpha(0.14)),
                        (1.0, col_.alpha(0.0)),
                    ],
                ),
        )
        .child(
            el().key("ring")
                .size(88.0, 88.0)
                .radius(44.0)
                .absolute(16.0, 16.0)
                .fill(oklch(0.16, 0.01, 275.0))
                .border(2.5, col_)
                .box_shadow(glow(col_, 30.0, 0.85)),
        )
        .child(symbol(&g.state, col_));
    if g.state == GateState::Running {
        core = core.cycle(Cycle {
            period: 1.3,
            phase: 0.0,
            stops: &[(0.0, 1.0), (0.5, 0.5), (1.0, 1.0)],
            still: 1.0,
            fps: 30.0,
        });
    }
    if fresh {
        // The verdict just landed: a shockwave leaves the core.
        core = core.child(
            el().key(format!("shock:{word}:{}", g.ts_ms.unwrap_or(0)))
                .size(88.0, 88.0)
                .radius(44.0)
                .absolute(16.0, 16.0)
                .border(2.0, col_)
                .sequence(Sequence::new(&SHOCK)),
        );
    }
    let text_block = col()
        .key("gate-text")
        .absolute(CORE[0] + 82.0, CORE[1] - 16.0)
        .w(280.0)
        .gap(3.0)
        .child(
            text("TEST GATE".to_owned())
                .font(k.ty.small_medium)
                .color(c.text_faint),
        )
        .child(
            text(word.to_owned())
                .key(format!("word:{word}"))
                .font(k.ty.body_medium)
                .color(col_)
                .transition(Transition::blur_replace()),
        )
        .child(text(source.to_owned()).font(k.ty.small).color(c.text_muted))
        .child(
            text(g.detail.clone())
                .font(k.ty.small)
                .color(c.text_faint)
                .wrap(),
        );
    el().key("reactor")
        .size(STAGE_W, STAGE_H)
        .absolute(0.0, 0.0)
        .child(core)
        .child(text_block)
}

/// The colour a CI verdict is drawn in. `None` (no run found) and cancelled are not colours of
/// success: they are hollow.
fn ci_color(p: &Pal, ci: Option<CiState>) -> Option<Color> {
    match ci {
        Some(CiState::Passed) => Some(p.ok),
        Some(CiState::Failed) => Some(p.fail),
        Some(CiState::Running) => Some(p.run),
        Some(CiState::Cancelled) | None => None,
    }
}

fn pulse(e: El, period: f32) -> El {
    e.cycle(Cycle {
        period,
        phase: 0.0,
        stops: &[(0.0, 1.0), (0.5, 0.4), (1.0, 1.0)],
        still: 1.0,
        fps: 30.0,
    })
}

/// The main track: one dot per commit, coloured by what CI reported for it. Newest at the head.
fn commit_nodes(dots: &[MainDot], culprit: Option<usize>, now: i64, k: &Kit, pal: &Pal) -> Vec<El> {
    let c = &k.c;
    let n = dots.len().min(MAX_COMMITS);
    let curve = main_curve();
    let mut out = Vec::new();
    for (i, cm) in dots.iter().take(MAX_COMMITS).enumerate() {
        // i = 0 is the newest: it sits at the head of the track, older ones trail back.
        let t = if n == 1 {
            0.95
        } else {
            0.95 - i as f32 * (0.8 / (n as f32 - 1.0))
        };
        let at = curve.at(t);
        let newest = i == 0;
        let size = if newest { 18.0 } else { 13.0 };
        let verdict = ci_color(pal, cm.ci);
        let body = verdict.unwrap_or(if newest { Color::WHITE } else { pal.dim });
        let mut e = el()
            .key(format!("commit:{}", cm.hash))
            .size(size, size)
            .absolute(at[0] - size / 2.0, at[1] - size / 2.0)
            .radius(size / 2.0)
            .transition(
                Transition::offset(40.0, -30.0)
                    .combined(Transition::scale(0.3))
                    .combined(Transition::blur(4.0)),
            );
        e = match (verdict, cm.ci) {
            // A verdict: a solid, glowing dot.
            (Some(vc), _) => {
                e.fill(vc)
                    .box_shadow(glow(vc, if newest { 24.0 } else { 12.0 }, 0.75))
            }
            // Cancelled: a hollow ring, so it cannot be mistaken for a pass.
            (None, Some(CiState::Cancelled)) => e.border(2.0, pal.faint),
            // No CI information at all: a plain dim dot, as before CI existed.
            (None, _) => e.fill(body).box_shadow(glow(
                body,
                if newest { 18.0 } else { 8.0 },
                if newest { 0.7 } else { 0.35 },
            )),
        };
        if cm.ci == Some(CiState::Running) {
            e = pulse(e, 1.2);
        }
        e = e.child(
            text(cm.hash.clone())
                .font(k.ty.mono)
                .color(if newest { c.text_base } else { c.text_faint })
                .absolute(-10.0, size + 6.0),
        );
        if newest && now - cm.ts_ms < FRESH_MS {
            e = e.child(ripple(format!("cmrip:{}", cm.hash), [0.0, 0.0], size, body));
        }
        if culprit == Some(i) {
            // The first red commit: a ring that keeps pulsing until it is fixed.
            e = e.child(pulse(
                el().key(format!("culprit-ring:{}", cm.hash))
                    .size(size + 16.0, size + 16.0)
                    .radius((size + 16.0) / 2.0)
                    .absolute(-8.0, -8.0)
                    .border(2.0, pal.fail)
                    .box_shadow(glow(pal.fail, 18.0, 0.8)),
                1.0,
            ));
            e = e.child(
                text("culprit".to_owned())
                    .font(k.ty.small_medium)
                    .color(pal.fail)
                    .absolute(-12.0, -34.0),
            );
        }
        out.push(e);
    }
    out
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
    if s < 90 {
        format!("{s}s ago")
    } else {
        format!("{} min ago", s / 60)
    }
}

/// Work waiting to land on the main track: the merge queue if the repo has one, else open pull
/// requests, else local branches that are ahead. Top right, above the gate text.
fn handoff_panel(b: &Board, k: &Kit, pal: &Pal) -> El {
    let c = &k.c;
    let (title, sub) = match (b.merge_queue, &b.link) {
        (Some(true), _) => ("MERGE QUEUE", "GitHub merge queue, in order"),
        (Some(false), _) => (
            "WAITING TO MERGE",
            "open PRs · this repo has no merge queue",
        ),
        (None, _) => ("BRANCHES AHEAD OF MAIN", "local worktrees ahead of main"),
    };
    let mut panel = col()
        .key("handoff")
        .absolute(738.0, 34.0)
        .w(246.0)
        .gap(4.0)
        .child(
            row()
                .items_center()
                .gap(8.0)
                .child(
                    text(title.to_owned())
                        .font(k.ty.small_medium)
                        .color(c.text_faint),
                )
                .child(
                    text(format!("{}", b.handoffs.len()))
                        .font(k.ty.small_medium)
                        .color(pal.accent),
                ),
        )
        .child(text(sub.to_owned()).font(k.ty.small).color(c.text_faint));
    if b.handoffs.is_empty() {
        return panel.child(
            text("nothing waiting".to_owned())
                .font(k.ty.small)
                .color(c.text_muted),
        );
    }
    for h in b.handoffs.iter().take(MAX_HANDOFFS) {
        panel = panel.child(handoff_chip(h, k, pal));
    }
    panel
}

fn handoff_chip(h: &Handoff, k: &Kit, pal: &Pal) -> El {
    let c = &k.c;
    let vc = ci_color(pal, h.ci);
    let mut dot = el().size(10.0, 10.0).radius(5.0);
    dot = match vc {
        Some(v) => dot.fill(v).box_shadow(glow(v, 10.0, 0.7)),
        None => dot.border(1.5, pal.faint),
    };
    if h.ci == Some(CiState::Running) {
        dot = pulse(dot, 1.2);
    }
    let tag = if let Some((pos, state)) = &h.queue {
        Some((
            format!(
                "#{pos} {}",
                clip(&state.to_lowercase().replace('_', " "), 9)
            ),
            pal.accent,
        ))
    } else if h.draft {
        Some(("draft".to_owned(), pal.faint))
    } else if h.auto_merge {
        Some(("auto-merge".to_owned(), pal.ok))
    } else {
        h.worktree.as_ref().map(|w| (clip(w, 10), pal.dim))
    };
    let mut r = row()
        .key(format!("ho:{}", h.label))
        .w_full()
        .items_center()
        .gap(8.0)
        .px(10.0)
        .py(4.0)
        .radius(14.0)
        .fill(Color::WHITE.alpha(0.05))
        .border(1.0, vc.unwrap_or(Color::WHITE.alpha(0.1)).alpha(0.45))
        .transition(
            Transition::offset(24.0, 0.0)
                .combined(Transition::opacity())
                .combined(Transition::blur(4.0)),
        )
        .child(dot)
        .child(text(clip(&h.label, 8)).font(k.ty.mono).color(c.text_base))
        .child(
            text(clip(&h.title, 15))
                .font(k.ty.small)
                .color(c.text_muted),
        );
    if let Some((t, tc)) = tag {
        r = r
            .child(el().grow(1.0))
            .child(text(t).font(k.ty.small).color(tc));
    }
    r
}

/// One line along the bottom: the culprit, or why there is nothing to say about CI.
fn status_strip(b: &Board, now: i64, k: &Kit, pal: &Pal) -> El {
    let c = &k.c;
    let mut stamp = String::new();
    let (word, tc, body): (&str, Color, String) = match &b.link {
        CiLink::NotChecked => (
            "CI",
            pal.faint,
            "not read (GitHub is off). Commit dots show no verdicts.".to_owned(),
        ),
        CiLink::Unavailable(why) => ("CI", pal.ask, format!("not shown. {why}")),
        CiLink::Live {
            slug,
            branch,
            fetched_ms,
        } => {
            stamp = format!("{slug}@{branch} · read {}", ago(now - fetched_ms));
            match &b.culprit {
                Some(cu) => {
                    let m = &b.main[cu.index];
                    let what = if m.failed_names.is_empty() {
                        "a run failed".to_owned()
                    } else {
                        format!("{} failed", m.failed_names.join(", "))
                    };
                    if cu.certain {
                        (
                            "CULPRIT",
                            pal.fail,
                            format!("{}  first red commit after the last green · {what}", m.hash),
                        )
                    } else {
                        (
                            "LIKELY CULPRIT",
                            pal.ask,
                            format!(
                                "{}  red, but {} newer-than-green commit(s) have no CI verdict",
                                m.hash, cu.unchecked
                            ),
                        )
                    }
                }
                None => {
                    let head = b.main.first().and_then(|m| m.ci);
                    let msg = match head {
                        Some(CiState::Passed) => "latest commit passed",
                        Some(CiState::Failed) => {
                            "latest commit failed; no green commit in view to compare"
                        }
                        Some(CiState::Running) => "latest commit still running",
                        _ => "no verdict for the latest commit",
                    };
                    (
                        "MAIN",
                        ci_color(pal, head).unwrap_or(pal.dim),
                        msg.to_owned(),
                    )
                }
            }
        }
    };
    row()
        .key("status-strip")
        .items_center()
        .absolute(250.0, 490.0)
        .w(736.0)
        .gap(10.0)
        .px(12.0)
        .py(5.0)
        .radius(12.0)
        .fill(tc.alpha(0.1))
        .border(1.0, tc.alpha(0.35))
        .child(text(word.to_owned()).font(k.ty.small_medium).color(tc))
        .child(text(clip(&body, 78)).font(k.ty.small).color(c.text_muted))
        .child(el().grow(1.0))
        .child(text(clip(&stamp, 40)).font(k.ty.small).color(c.text_faint))
}

pub fn view(
    p: &Pipeline,
    board: &Board,
    synthetic: bool,
    note: Option<&str>,
    now: i64,
    k: &Kit,
) -> El {
    let c = &k.c;
    let pal = pal();
    let enter = || {
        Transition::offset(0.0, 18.0)
            .combined(Transition::opacity())
            .combined(Transition::blur(6.0))
    };

    let mut head = col()
        .w_full()
        .gap(2.0)
        .px(4.0)
        .child(text("Pipeline".to_owned()).font(k.ty.body_medium).color(c.text_base))
        .child(
            text("Live from recorded Claude events, test runs and git. Motion happens only when something real does.".to_owned())
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

    // The most recently active sessions, drawn in a stable order so curves never swap places.
    let mut shown: Vec<&Lane> = p.lanes.iter().take(MAX_LANES).collect();
    shown.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let n = shown.len();

    let mut stage = el()
        .key("stage")
        .size(STAGE_W, STAGE_H)
        .radius(20.0)
        .clip_rounded(20.0)
        .fill(oklch(0.15, 0.012, 280.0))
        .radial_gradient(
            [0.64, 0.34],
            [620.0, 380.0],
            [
                (0.0, pal.accent.alpha(0.2)),
                (0.6, pal.accent.alpha(0.05)),
                (1.0, pal.accent.alpha(0.0)),
            ],
        )
        .border(1.0, Color::WHITE.alpha(0.07))
        .motion(motion::MORPH)
        .transition(enter());
    for st in stars() {
        stage = stage.child(st);
    }
    stage = stage.child(wires(&shown, n, &pal));
    if shown.is_empty() {
        stage = stage.child(
            text(
                "No sessions yet. Orbs appear here when the Station plugin reports events."
                    .to_owned(),
            )
            .font(k.ty.small)
            .color(c.text_muted)
            .absolute(40.0, 160.0),
        );
    }
    for (i, l) in shown.iter().enumerate() {
        stage = lane_parts(stage, l, i, n, now, k, &pal);
    }
    stage = stage.child(reactor(&p.gate, now, k, &pal));
    // Until the first board read lands, fall back to the plain git log from the pipeline model.
    let dots: Vec<MainDot> = if board.main.is_empty() {
        p.commits
            .iter()
            .map(|c| MainDot {
                hash: c.hash.clone(),
                subject: c.subject.clone(),
                ts_ms: c.ts_ms,
                ci: None,
                failed_names: Vec::new(),
                runs: Vec::new(),
            })
            .collect()
    } else {
        board.main.clone()
    };
    for e in commit_nodes(&dots, board.culprit.as_ref().map(|c| c.index), now, k, &pal) {
        stage = stage.child(e);
    }
    if !(dots.is_empty() && board.handoffs.is_empty()) {
        stage = stage.child(handoff_panel(board, k, &pal));
    }
    if !dots.is_empty() {
        stage = stage.child(status_strip(board, now, k, &pal));
    }
    let caption = match (dots.len(), board.base.as_deref()) {
        (0, _) => "No commits to show (no project, or an empty repository).".to_owned(),
        (n, Some(b)) => format!("main track: {} newest commits of {b}", n.min(MAX_COMMITS)),
        (n, None) => format!("main track: {} newest commits", n.min(MAX_COMMITS)),
    };
    stage = stage.child(text(caption).font(k.ty.small).color(c.text_faint).absolute(24.0, 494.0)).child(
        text("ring prompt · rounded square edit · orb command · diamond test-looking (dashed = inferred) · green/red result · blue pulse running · amber needs you".to_owned())
            .font(k.ty.small)
            .color(c.text_faint)
            .absolute(24.0, 20.0),
    );

    col()
        .key("pipeline")
        .w_full()
        .h_full()
        .padding(20.0)
        .gap(14.0)
        .child(head)
        .child(
            row()
                .w_full()
                .child(el().grow(1.0))
                .child(stage)
                .child(el().grow(1.0)),
        )
}
