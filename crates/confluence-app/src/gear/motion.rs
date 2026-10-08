//! The motion system: every tween, spring, phosphor decay and meter
//! ballistic the window animates lives here, keyed by widget id, so one
//! place decides whether anything is still moving and asks for the next
//! frame only then. Physical things (panels, pills, cards) settle on springs;
//! electronic things (LEDs, OLED text) step on and decay off like phosphor;
//! meters jump up and fall at a fixed rate. With nothing moving the window
//! draws no frames at all.

use std::collections::HashMap;
use std::time::Duration;

use eframe::egui::{self, Id};

/// A spring's stiffness and damping (mass 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpringParams {
    pub stiffness: f32,
    pub damping: f32,
}

/// Critically damped: settles in about 320 ms with no overshoot. Card lift,
/// pill press, knob values, highlights moving.
pub const SETTLE: SpringParams = SpringParams { stiffness: 220.0, damping: 29.66 };
/// One small overshoot (about 6 %): a route being made, a card appearing.
pub const POP: SpringParams = SpringParams { stiffness: 300.0, damping: 23.0 };

/// An easing curve for tweens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    /// Emphasised decelerate, cubic-bezier(0.2, 0, 0, 1): things appearing.
    Enter,
    /// cubic-bezier(0.3, 0, 0.8, 0.15): things leaving.
    Exit,
    Linear,
}

/// Durations of the two tweens.
pub const ENTER: f32 = 0.220;
pub const EXIT: f32 = 0.140;
/// Phosphor fall-off time constant (LED off, OLED text dimming).
pub const PHOSPHOR_TAU: f32 = 0.090;
/// Meter release time constant: about 20 dB/s over the top 10 dB.
pub const PPM_TAU: f32 = 0.180;
/// Peak hold before it falls, and how fast it falls.
pub const HOLD_SECS: f32 = 1.5;
pub const HOLD_FALL_DB_PER_S: f32 = 24.0;
/// Below this a meter channel is silent and its ballistics stop.
pub const SILENT_DB: f32 = -90.0;
/// The next frame is asked for this soon while anything moves.
pub const FRAME: Duration = Duration::from_millis(8);

/// The y of a cubic bezier (0,0)-(x1,y1)-(x2,y2)-(1,1) at `s` along x.
pub fn bezier(x1: f32, y1: f32, x2: f32, y2: f32, s: f32) -> f32 {
    let s = s.clamp(0.0, 1.0);
    let at = |t: f32, a: f32, b: f32| 3.0 * (1.0 - t).powi(2) * t * a + 3.0 * (1.0 - t) * t * t * b + t * t * t;
    // x(t) is monotonic on 0..1 for these control points: bisect for t.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if at(mid, x1, x2) < s {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    at((lo + hi) / 2.0, y1, y2)
}

impl Curve {
    pub fn at(self, s: f32) -> f32 {
        match self {
            Curve::Enter => bezier(0.2, 0.0, 0.0, 1.0, s),
            Curve::Exit => bezier(0.3, 0.0, 0.8, 0.15, s),
            Curve::Linear => s.clamp(0.0, 1.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub params: SpringParams,
    seen: u64,
}

impl Spring {
    /// Advances by `dt` seconds (substepped for stability); true if it still moves.
    pub fn step(&mut self, dt: f32) -> bool {
        let mut left = dt.clamp(0.0, 0.1);
        while left > 0.0 {
            let h = left.min(0.004);
            let a = -self.params.stiffness * (self.value - self.target) - self.params.damping * self.velocity;
            self.velocity += a * h;
            self.value += self.velocity * h;
            left -= h;
        }
        if (self.value - self.target).abs() < 0.002 && self.velocity.abs() < 0.03 {
            self.value = self.target;
            self.velocity = 0.0;
            return false;
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tween {
    pub from: f32,
    pub to: f32,
    pub start: f64,
    pub secs: f32,
    pub curve: Curve,
    seen: u64,
}

impl Tween {
    pub fn value_at(&self, now: f64) -> f32 {
        if self.secs <= 0.0 {
            return self.to;
        }
        let s = ((now - self.start) as f32 / self.secs).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * self.curve.at(s)
    }

    pub fn done_at(&self, now: f64) -> bool {
        now - self.start >= self.secs as f64
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Decay {
    pub value: f32,
    pub target: f32,
    pub tau: f32,
    seen: u64,
}

impl Decay {
    pub fn step(&mut self, dt: f32) -> bool {
        let k = 1.0 - (-dt / self.tau.max(1e-3)).exp();
        self.value += (self.target - self.value) * k;
        if (self.value - self.target).abs() < 0.002 {
            self.value = self.target;
            return false;
        }
        true
    }
}

/// One meter channel's ballistics: the level jumps up and falls with
/// [`PPM_TAU`]; the peak holds [`HOLD_SECS`] then falls at
/// [`HOLD_FALL_DB_PER_S`]. Values in dB; silence is [`SILENT_DB`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ppm {
    pub level: f32,
    pub hold: f32,
    /// When the held peak was set.
    pub held_at: f64,
    seen: u64,
}

impl Ppm {
    pub fn new(now: f64) -> Ppm {
        Ppm { level: SILENT_DB, hold: SILENT_DB, held_at: now, seen: 0 }
    }

    /// Feeds the engine's value and advances by `dt`; true while anything
    /// is still above silence or still falling.
    pub fn step(&mut self, target: f32, now: f64, dt: f32) -> bool {
        let target = if target.is_finite() { target.max(SILENT_DB) } else { SILENT_DB };
        if target >= self.level {
            self.level = target;
        } else {
            let k = 1.0 - (-dt / PPM_TAU).exp();
            self.level += (target - self.level) * k;
            if self.level - target < 0.05 {
                self.level = target;
            }
        }
        if self.level >= self.hold {
            self.hold = self.level;
            self.held_at = now;
        } else if now - self.held_at > HOLD_SECS as f64 {
            self.hold = (self.hold - HOLD_FALL_DB_PER_S * dt).max(self.level);
        }
        self.level > SILENT_DB || self.hold > SILENT_DB
    }
}

/// Every animation in flight, and whether the next frame is needed.
pub struct Motion {
    springs: HashMap<Id, Spring>,
    tweens: HashMap<Id, Tween>,
    decays: HashMap<Id, Decay>,
    meters: HashMap<Id, Ppm>,
    /// Frames counted by [`Motion::begin`].
    frame: u64,
    now: f64,
    dt: f32,
    /// Something asked for a frame this soon (blinks, settling motion).
    next: Option<Duration>,
    /// Reduce motion: tweens and springs land at once; meters, LEDs and
    /// blinks keep their electronic timing.
    pub reduce: bool,
}

impl Default for Motion {
    fn default() -> Self {
        Motion {
            springs: HashMap::new(),
            tweens: HashMap::new(),
            decays: HashMap::new(),
            meters: HashMap::new(),
            frame: 0,
            now: 0.0,
            dt: 0.0,
            next: None,
            reduce: false,
        }
    }
}

impl Motion {
    /// Starts a frame at `now` seconds, `dt` since the last one: advances
    /// every spring, decay and meter.
    pub fn begin(&mut self, now: f64, dt: f32) {
        self.frame += 1;
        self.now = now;
        self.dt = dt.clamp(0.0, 0.1);
        self.next = None;
        let dt = self.dt;
        let mut moving = false;
        for s in self.springs.values_mut() {
            moving |= s.step(dt);
        }
        for d in self.decays.values_mut() {
            moving |= d.step(dt);
        }
        for t in self.tweens.values() {
            moving |= !t.done_at(now);
        }
        if moving {
            self.ask(FRAME);
        }
    }

    /// Starts a frame from an egui context.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        let (now, dt) = ctx.input(|i| (i.time, i.stable_dt));
        self.begin(now, dt);
    }

    /// Ends the frame: forgets animations nothing drew, and asks for the
    /// next frame if anything still moves.
    pub fn end(&mut self) -> Option<Duration> {
        let frame = self.frame;
        self.springs.retain(|_, s| s.seen == frame);
        self.tweens.retain(|_, t| t.seen == frame);
        self.decays.retain(|_, d| d.seen == frame);
        self.meters.retain(|_, m| m.seen == frame);
        self.next
    }

    pub fn end_frame(&mut self, ctx: &egui::Context) {
        if let Some(after) = self.end() {
            ctx.request_repaint_after(after);
        }
    }

    fn ask(&mut self, after: Duration) {
        self.next = Some(self.next.map_or(after, |n| n.min(after)));
    }

    pub fn now(&self) -> f64 {
        self.now
    }

    /// Asks for the next frame soon (something is fading on its own clock).
    pub fn wake(&mut self) {
        self.ask(FRAME);
    }

    /// True while any animation is in flight.
    pub fn active(&self) -> bool {
        self.next.is_some()
    }

    /// A value that settles toward `target` on a spring. A new id starts at
    /// `target` (nothing animates on first appearance).
    pub fn spring(&mut self, id: Id, target: f32, params: SpringParams) -> f32 {
        self.spring_from(id, target, target, params)
    }

    /// As [`Motion::spring`], but a new id starts at `from` and springs to `target`.
    pub fn spring_from(&mut self, id: Id, from: f32, target: f32, params: SpringParams) -> f32 {
        let frame = self.frame;
        let reduce = self.reduce;
        let s = self.springs.entry(id).or_insert(Spring { value: from, velocity: 0.0, target, params, seen: frame });
        s.seen = frame;
        s.target = target;
        s.params = params;
        if reduce {
            s.value = target;
            s.velocity = 0.0;
        }
        let (moving, value) = (s.value != s.target || s.velocity != 0.0, s.value);
        if moving {
            self.ask(FRAME);
        }
        value
    }

    /// A value that eases toward `target` over `secs` with `curve`; a new
    /// target restarts the tween from the current value. A new id starts at
    /// `target`.
    pub fn tween(&mut self, id: Id, target: f32, curve: Curve, secs: f32) -> f32 {
        self.tween_from(id, target, target, curve, secs)
    }

    /// As [`Motion::tween`], but a new id starts at `from`.
    pub fn tween_from(&mut self, id: Id, from: f32, target: f32, curve: Curve, secs: f32) -> f32 {
        let (frame, now) = (self.frame, self.now);
        let secs = if self.reduce { 0.0 } else { secs };
        let t = self.tweens.entry(id).or_insert(Tween { from, to: target, start: now, secs, curve, seen: frame });
        t.seen = frame;
        if t.to != target {
            let cur = t.value_at(now);
            *t = Tween { from: cur, to: target, start: now, secs, curve, seen: frame };
        }
        let v = t.value_at(now);
        if !t.done_at(now) {
            self.ask(FRAME);
        }
        v
    }

    /// A value that steps up to `target` at once and decays down toward it
    /// with time constant `tau` (phosphor): LEDs, OLED glow.
    pub fn phosphor(&mut self, id: Id, target: f32, tau: f32) -> f32 {
        let frame = self.frame;
        let d = self.decays.entry(id).or_insert(Decay { value: target, target, tau, seen: frame });
        d.seen = frame;
        d.tau = tau;
        d.target = target;
        if target > d.value {
            d.value = target;
        }
        let (moving, value) = (d.value != d.target, d.value);
        if moving {
            self.ask(FRAME);
        }
        value
    }

    /// A value that decays toward `target` both ways (used for fades that
    /// should not step).
    pub fn decay(&mut self, id: Id, target: f32, tau: f32) -> f32 {
        let frame = self.frame;
        let d = self.decays.entry(id).or_insert(Decay { value: target, target, tau, seen: frame });
        d.seen = frame;
        d.tau = tau;
        d.target = target;
        if self.reduce {
            d.value = target;
        }
        let (moving, value) = (d.value != d.target, d.value);
        if moving {
            self.ask(FRAME);
        }
        value
    }

    /// A meter channel fed `target_db`: (level, held peak) in dB.
    pub fn ppm(&mut self, id: Id, target_db: f32) -> (f32, f32) {
        let (frame, now, dt) = (self.frame, self.now, self.dt);
        let m = self.meters.entry(id).or_insert_with(|| Ppm::new(now));
        m.seen = frame;
        let moving = m.step(target_db, now, dt);
        let out = (m.level, m.hold);
        if moving {
            self.ask(FRAME);
        }
        out
    }

    /// A square-wave blink at `hz`: true for the first half of each period.
    /// Asks for a frame at the next edge.
    pub fn blink(&mut self, hz: f32) -> bool {
        let period = 1.0 / hz.max(0.01) as f64;
        let phase = (self.now / period).fract();
        let until = (if phase < 0.5 { 0.5 - phase } else { 1.0 - phase }) * period;
        self.ask(Duration::from_secs_f64(until.max(0.004)));
        phase < 0.5
    }

    /// A smooth pulse at `hz` (0..1, a raised cosine).
    pub fn pulse(&mut self, hz: f32) -> f32 {
        self.ask(FRAME);
        let phase = (self.now * hz as f64).fract() as f32;
        0.5 - 0.5 * (phase * std::f32::consts::TAU).cos()
    }

    /// A one-shot progress 0..1 that started at `since` and lasts `secs`
    /// (e.g. a flash); asks for frames until it ends.
    pub fn since(&mut self, since: f64, secs: f32) -> f32 {
        let s = ((self.now - since) as f32 / secs.max(1e-3)).clamp(0.0, 1.0);
        if s < 1.0 {
            self.ask(FRAME);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(m: &mut Motion, frames: usize, dt: f32, mut f: impl FnMut(&mut Motion) -> f32) -> f32 {
        let mut v = 0.0;
        for k in 0..frames {
            m.begin(k as f64 * dt as f64, dt);
            v = f(m);
            m.end();
        }
        v
    }

    #[test]
    fn the_curves_start_at_zero_and_end_at_one_monotonically() {
        for c in [Curve::Enter, Curve::Exit, Curve::Linear] {
            assert!(c.at(0.0).abs() < 1e-4, "{c:?}");
            assert!((c.at(1.0) - 1.0).abs() < 1e-4, "{c:?}");
            let mut last = -1.0;
            for i in 0..=50 {
                let y = c.at(i as f32 / 50.0);
                assert!(y >= last - 1e-5, "{c:?} dips at {i}");
                last = y;
            }
        }
        assert!(Curve::Enter.at(0.3) > 0.6, "enter decelerates: most of the way early");
        assert!(Curve::Exit.at(0.5) < 0.3, "exit accelerates: little way at half time");
    }

    #[test]
    fn a_settle_spring_lands_in_about_a_third_of_a_second_without_overshoot() {
        let mut s = Spring { value: 0.0, velocity: 0.0, target: 1.0, params: SETTLE, seen: 0 };
        let (mut t, mut max) = (0.0f32, 0.0f32);
        while s.step(1.0 / 120.0) {
            t += 1.0 / 120.0;
            max = max.max(s.value);
            assert!(t < 2.0, "never settled");
        }
        assert!((0.15..0.7).contains(&t), "settled after {t} s");
        assert!(max <= 1.0 + 1e-3, "overshot to {max}");
        assert_eq!(s.value, 1.0, "snapped to the target");
    }

    #[test]
    fn a_pop_spring_overshoots_once_by_a_few_percent() {
        let mut s = Spring { value: 0.6, velocity: 0.0, target: 1.0, params: POP, seen: 0 };
        let mut max = 0.0f32;
        let mut t = 0.0;
        while s.step(1.0 / 120.0) {
            t += 1.0 / 120.0;
            max = max.max(s.value);
            assert!(t < 2.0);
        }
        let over = (max - 1.0) / 0.4;
        assert!((0.02..0.12).contains(&over), "overshoot {over}");
        assert!(t < 0.7, "settled after {t} s");
    }

    #[test]
    fn a_tween_follows_its_curve_and_restarts_from_where_it_is() {
        let mut m = Motion::default();
        let id = Id::new("t");
        m.begin(0.0, 0.0);
        assert_eq!(m.tween(id, 0.0, Curve::Linear, 1.0), 0.0, "a new id starts at its target");
        m.end();
        m.begin(0.1, 0.1);
        let v = m.tween(id, 1.0, Curve::Linear, 1.0);
        assert_eq!(v, 0.0, "a new target starts from the current value");
        assert!(m.active());
        m.end();
        m.begin(0.6, 0.5);
        assert!((m.tween(id, 1.0, Curve::Linear, 1.0) - 0.5).abs() < 1e-4);
        m.end();
        m.begin(0.7, 0.1);
        let back = m.tween(id, 0.0, Curve::Linear, 1.0);
        assert!((back - 0.6).abs() < 1e-4, "reversing starts from 0.6: {back}");
        m.end();
    }

    #[test]
    fn phosphor_steps_up_at_once_and_decays_down() {
        let mut m = Motion::default();
        let id = Id::new("led");
        m.begin(0.0, 0.0);
        assert_eq!(m.phosphor(id, 0.0, PHOSPHOR_TAU), 0.0);
        m.end();
        m.begin(0.016, 0.016);
        assert_eq!(m.phosphor(id, 1.0, PHOSPHOR_TAU), 1.0, "on is a step");
        assert!(!m.active(), "nothing moves after a step");
        m.end();
        m.begin(0.032, 0.016);
        let v = m.phosphor(id, 0.0, PHOSPHOR_TAU);
        assert_eq!(v, 1.0, "the target changed this frame; the decay shows next frame");
        assert!(m.active());
        m.end();
        let v = run(&mut m, 3, 0.09, |m| m.phosphor(id, 0.0, PHOSPHOR_TAU));
        assert!(v > 0.0 && v < 0.2, "after about three time constants: {v}");
    }

    #[test]
    fn meters_jump_up_and_fall_at_about_twenty_db_per_second() {
        let mut p = Ppm::new(0.0);
        assert!(p.step(-6.0, 0.0, 0.016));
        assert_eq!(p.level, -6.0, "attack is instant");
        assert_eq!(p.hold, -6.0);
        // Silence: after one second the level has fallen most of the way to silence.
        let mut now = 0.0;
        for _ in 0..60 {
            now += 1.0 / 60.0;
            p.step(SILENT_DB, now, 1.0 / 60.0);
        }
        assert!(p.level < -70.0, "fell to {} after 1 s", p.level);
        assert_eq!(p.hold, -6.0, "the peak still holds at 1 s");
        for _ in 0..60 {
            now += 1.0 / 60.0;
            p.step(SILENT_DB, now, 1.0 / 60.0);
        }
        let fell = -6.0 - p.hold;
        assert!((8.0..16.0).contains(&fell), "the hold fell {fell} dB in the half second after holding");
        for _ in 0..600 {
            now += 1.0 / 60.0;
            if !p.step(SILENT_DB, now, 1.0 / 60.0) {
                break;
            }
        }
        assert!(!p.step(SILENT_DB, now, 1.0 / 60.0), "a silent meter stops asking for frames");
    }

    #[test]
    fn no_frame_is_asked_for_once_everything_has_settled() {
        let mut m = Motion::default();
        let id = Id::new("lift");
        m.begin(0.0, 0.0);
        m.spring(id, 0.0, SETTLE);
        assert_eq!(m.end(), None, "a new spring at its target asks for nothing");
        m.begin(0.016, 0.016);
        m.spring(id, 1.0, SETTLE);
        assert_eq!(m.end(), Some(FRAME), "moving: the next frame soon");
        let mut asked = 0;
        for k in 2..200 {
            m.begin(k as f64 * 0.016, 0.016);
            m.spring(id, 1.0, SETTLE);
            if m.end().is_some() {
                asked += 1;
            }
        }
        assert!(asked > 5 && asked < 60, "asked for {asked} frames while settling");
        m.begin(10.0, 0.016);
        assert_eq!(m.spring(id, 1.0, SETTLE), 1.0);
        assert_eq!(m.end(), None, "settled: idle");
    }

    #[test]
    fn forgotten_ids_are_dropped_and_reduce_motion_lands_at_once() {
        let mut m = Motion::default();
        m.begin(0.0, 0.0);
        m.spring(Id::new("a"), 0.0, SETTLE);
        m.end();
        m.begin(0.016, 0.016);
        m.end();
        assert!(m.springs.is_empty(), "an id nothing drew is forgotten");
        m.reduce = true;
        m.begin(0.032, 0.016);
        m.spring(Id::new("b"), 0.0, SETTLE);
        m.end();
        m.begin(0.048, 0.016);
        assert_eq!(m.spring(Id::new("b"), 1.0, SETTLE), 1.0);
        assert_eq!(m.tween_from(Id::new("c"), 0.0, 1.0, Curve::Enter, ENTER), 1.0);
        assert_eq!(m.end(), None);
    }

    #[test]
    fn a_blink_asks_for_the_next_edge_not_every_frame() {
        let mut m = Motion::default();
        m.begin(0.1, 0.016);
        assert!(m.blink(2.0), "first half of a 500 ms period");
        let next = m.end().unwrap_or_default();
        assert!((next.as_secs_f64() - 0.15).abs() < 0.01, "{next:?}");
        m.begin(0.3, 0.2);
        assert!(!m.blink(2.0));
    }

    /// The whole point: an egui frame in which every animation has settled
    /// asks for no repaint.
    #[test]
    fn an_idle_egui_frame_requests_no_repaint() {
        let ctx = egui::Context::default();
        let mut m = Motion::default();
        let id = Id::new("x");
        let mut input = egui::RawInput::default();
        let mut frame = |m: &mut Motion, target: f32, t: f64| {
            input.time = Some(t);
            let mut out = ctx.run_ui(input.clone(), |ui| {
                let ctx = ui.ctx().clone();
                m.begin_frame(&ctx);
                m.spring(id, target, SETTLE);
                m.end_frame(&ctx);
            });
            out.textures_delta.clear(); // a frame's texture changes must be handled
            out.viewport_output.get(&egui::ViewportId::ROOT).map(|v| v.repaint_delay)
        };
        frame(&mut m, 0.0, 0.0);
        let moving = frame(&mut m, 1.0, 0.016);
        assert!(moving.is_some_and(|d| d <= FRAME), "{moving:?}");
        for k in 2..120 {
            frame(&mut m, 1.0, k as f64 * 0.016);
        }
        let idle = frame(&mut m, 1.0, 5.0);
        assert_eq!(idle, Some(Duration::MAX), "settled: no repaint requested");
    }
}
