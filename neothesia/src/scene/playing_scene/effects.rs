//! Guitar-Hero-style visual effects for play-along mode — the "dream big" edition.
//!
//! On a correct hit we blast a **light pillar** up the note lane, throw a fat
//! burst of glowing sparks (each with a soft bloom halo), pop a big flash ring
//! on the key, and bump a running combo / x1..x4 multiplier. Every 10 hits
//! fires a full milestone explosion + screen flash. Pass a 15 streak and the
//! keyboard is "on fire" — tall ambient flames rise from the hit line. A wrong
//! note breaks the combo with a grey puff.
//!
//! Everything draws as plain rounded quads through the existing foreground
//! [`QuadRenderer`] (bloom faked with additive-ish translucent halos over the
//! dark background), so there is no extra GPU pipeline. Nothing fires unless a
//! track is set to "Human", so ordinary auto-play is untouched.

use std::collections::VecDeque;

use neothesia_core::render::{QuadInstance, QuadRenderer};

const GRAVITY: f32 = 1350.0;
const MAX_PARTICLES: usize = 6000;
const FIRE_COMBO: u32 = 15;
/// How many recent notes the "audience" judges you on.
const SENTIMENT_WINDOW: usize = 20;
/// Hit-timing grades (seconds between the file note and your press).
const PERFECT_WINDOW: f32 = 0.15;
const GOOD_WINDOW: f32 = 0.35;

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Spark,
    Ember,
    Flash,
    Beam,
    Puff,
}

struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    life: f32,
    max_life: f32,
    /// Circle diameter for sparks/embers/flash; bar width for beams.
    size: f32,
    /// Beam length (unused by other styles).
    length: f32,
    /// Linear RGB (can exceed 1.0 to read as "hot" through blending).
    color: [f32; 3],
    gravity: f32,
    style: Style,
}

/// A falling note bar that was struck correctly and is currently lit up.
/// Geometry is kept in song-time coordinates and converted to screen space
/// every frame with the same math as the waterfall shader.
struct NoteFlash {
    x: f32,
    w: f32,
    /// Note start, in song seconds.
    start: f32,
    /// Bar height in seconds (matches the waterfall instance: max(dur,0.1)-0.01).
    h: f32,
    life: f32,
    max_life: f32,
    color: [f32; 3],
}

/// "PERFECT!" / "GOOD" text rising out of the top of a struck key.
pub struct RisingText {
    x: f32,
    /// Key top (hit line) — the text rises up from here.
    y0: f32,
    age: f32,
    life: f32,
    pub perfect: bool,
}

impl RisingText {
    pub fn text(&self) -> &'static str {
        if self.perfect { "PERFECT!" } else { "GOOD" }
    }

    pub fn x(&self) -> f32 {
        self.x
    }

    pub fn y(&self) -> f32 {
        let rise = if self.perfect { 110.0 } else { 80.0 };
        self.y0 - 24.0 - self.age * rise
    }

    pub fn alpha(&self) -> f32 {
        (1.0 - self.age / self.life).clamp(0.0, 1.0).powf(0.8)
    }

    pub fn font_size(&self) -> f32 {
        let base = if self.perfect { 24.0 } else { 19.0 };
        // Quick pop at birth.
        base * (1.0 + 0.35 * (1.0 - (self.age * 6.0).min(1.0)))
    }
}

pub struct EffectsSystem {
    particles: Vec<Particle>,
    note_flashes: Vec<NoteFlash>,
    rising: Vec<RisingText>,
    /// Rolling record of the last few notes: `true` = correct, `false` = wrong.
    recent: VecDeque<bool>,
    rng: u64,

    combo: u32,
    best_combo: u32,
    combo_pop: f32,
    ember_acc: f32,

    /// Smoothed 0..1 sentiment shown by the dial needle (eases toward the
    /// rolling accuracy so it sweeps like a real gauge).
    sentiment_display: f32,

    // Whole-song tallies for the results screen.
    total_perfect: u32,
    total_good: u32,
    total_ok: u32,
    total_wrong: u32,

    /// Timer for celebration fireworks on the results screen.
    celebrate_acc: f32,
}

/// Whole-song performance summary for the results screen.
#[derive(Clone, Copy)]
pub struct Results {
    pub perfect: u32,
    pub good: u32,
    pub ok: u32,
    pub wrong: u32,
    pub best_combo: u32,
}

impl Results {
    pub fn total_hit(&self) -> u32 {
        self.perfect + self.good + self.ok
    }

    pub fn accuracy(&self) -> f32 {
        let total = self.total_hit() + self.wrong;
        if total == 0 {
            return 0.0;
        }
        self.total_hit() as f32 / total as f32
    }

    /// Arcade letter grade with its display colour.
    pub fn grade(&self) -> (&'static str, (u8, u8, u8)) {
        let acc = self.accuracy();
        if acc >= 0.97 {
            ("S", (255, 200, 40))
        } else if acc >= 0.90 {
            ("A", (80, 220, 90))
        } else if acc >= 0.75 {
            ("B", (70, 140, 255))
        } else if acc >= 0.60 {
            ("C", (255, 150, 50))
        } else {
            ("D", (230, 60, 50))
        }
    }

    /// Does this performance deserve fireworks?
    pub fn celebratory(&self) -> bool {
        self.accuracy() >= 0.90
    }
}

impl EffectsSystem {
    pub fn new() -> Self {
        Self {
            particles: Vec::new(),
            note_flashes: Vec::new(),
            rising: Vec::new(),
            recent: VecDeque::new(),
            rng: 0x9E3779B97F4A7C15,
            combo: 0,
            best_combo: 0,
            combo_pop: 0.0,
            ember_acc: 0.0,
            sentiment_display: 0.5,
            total_perfect: 0,
            total_good: 0,
            total_ok: 0,
            total_wrong: 0,
            celebrate_acc: 0.0,
        }
    }

    pub fn results(&self) -> Results {
        Results {
            perfect: self.total_perfect,
            good: self.total_good,
            ok: self.total_ok,
            wrong: self.total_wrong,
            best_combo: self.best_combo,
        }
    }

    /// Rolling accuracy over the sentiment window, if anything was played.
    fn accuracy(&self) -> Option<f32> {
        if self.recent.is_empty() {
            return None;
        }
        let good = self.recent.iter().filter(|g| **g).count() as f32;
        Some(good / self.recent.len() as f32)
    }

    fn record_result(&mut self, good: bool) {
        if self.recent.len() >= SENTIMENT_WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(good);
    }

    // --- tiny xorshift PRNG --------------------------------------------------

    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    fn rand(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn rand_range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.rand()
    }

    fn room(&self) -> bool {
        self.particles.len() < MAX_PARTICLES
    }

    // --- HUD state -----------------------------------------------------------

    pub fn combo(&self) -> u32 {
        self.combo
    }

    /// Longest streak this session.
    pub fn best_combo(&self) -> u32 {
        self.best_combo
    }

    pub fn multiplier(&self) -> u32 {
        (1 + self.combo / 10).min(4)
    }

    pub fn combo_pop(&self) -> f32 {
        self.combo_pop
    }

    pub fn on_fire(&self) -> bool {
        self.combo >= FIRE_COMBO
    }

    /// True once the player has actually played something this song.
    pub fn has_activity(&self) -> bool {
        !self.recent.is_empty()
    }

    pub fn rising_texts(&self) -> impl Iterator<Item = &RisingText> {
        self.rising.iter()
    }

    /// Audience sentiment over the last [`SENTIMENT_WINDOW`] notes, as a level
    /// 0..=5: 0 = angry, 2 = neutral, 4 = big smile, 5 = star-eyed grin.
    /// `None` until a few notes have been judged.
    pub fn sentiment_level(&self) -> Option<u8> {
        if self.recent.len() < 3 {
            return None;
        }

        let accuracy = self.accuracy()?;

        Some(if accuracy >= 0.95 && self.recent.len() >= 10 {
            5
        } else if accuracy >= 0.85 {
            4
        } else if accuracy >= 0.7 {
            3
        } else if accuracy >= 0.5 {
            2
        } else if accuracy >= 0.3 {
            1
        } else {
            0
        })
    }

    // --- event hooks ---------------------------------------------------------

    /// Correct note. `cx` = key centre, `y` = hit line (keyboard top, which is
    /// also the height of the note lane above it), `key_w` = white-key width,
    /// `delta_secs` = timing gap between the file note and the press.
    pub fn good_hit(&mut self, note_id: u8, cx: f32, y: f32, key_w: f32, delta_secs: f32) {
        self.combo += 1;
        self.best_combo = self.best_combo.max(self.combo);
        self.combo_pop = 1.0;
        self.record_result(true);

        if delta_secs <= PERFECT_WINDOW {
            self.total_perfect += 1;
        } else if delta_secs <= GOOD_WINDOW {
            self.total_good += 1;
        } else {
            self.total_ok += 1;
        }

        // Timing grade text rising out of the key.
        if delta_secs <= GOOD_WINDOW {
            self.rising.push(RisingText {
                x: cx,
                y0: y,
                age: 0.0,
                life: 0.9,
                perfect: delta_secs <= PERFECT_WINDOW,
            });
        }

        let mult = self.multiplier() as f32;
        let base = note_linear_color(note_id);
        let milestone = self.combo % 10 == 0;

        // 1) Soft light pillar up the note lane.
        if self.room() {
            let lane = (y * 0.8).clamp(120.0, 900.0);
            self.particles.push(Particle {
                x: cx,
                y,
                vx: 0.0,
                vy: 0.0,
                life: if milestone { 0.45 } else { 0.32 },
                max_life: if milestone { 0.45 } else { 0.32 },
                size: key_w * if milestone { 1.1 } else { 0.8 },
                length: lane,
                color: mix(base, [2.0, 2.0, 1.9], 0.5),
                gravity: 0.0,
                style: Style::Beam,
            });
        }

        // 2) Big flash ring on the key.
        if self.room() {
            self.particles.push(Particle {
                x: cx,
                y,
                vx: 0.0,
                vy: 0.0,
                life: if milestone { 0.42 } else { 0.26 },
                max_life: if milestone { 0.42 } else { 0.26 },
                size: key_w * if milestone { 5.5 } else { 3.2 },
                length: 0.0,
                color: mix(base, [2.2, 2.2, 2.0], 0.6),
                gravity: 0.0,
                style: Style::Flash,
            });
        }

        // 3) Fat glowing spark burst.
        let count = (34.0 + mult * 12.0 + if milestone { 70.0 } else { 0.0 }) as usize;
        for _ in 0..count {
            if !self.room() {
                break;
            }
            let speed = self.rand_range(300.0, 900.0) * (0.85 + 0.09 * mult);
            let dirx = self.rand_range(-1.0, 1.0);
            let diry = self.rand_range(-1.0, -0.22); // upward bias
            let len = (dirx * dirx + diry * diry).sqrt().max(0.0001);
            let life = self.rand_range(0.55, 1.3);
            let x_off = self.rand_range(-key_w * 0.25, key_w * 0.25);
            let size = self.rand_range(7.0, 18.0);
            let hot = self.rand() * 0.7;
            let color = mix(base, [2.2, 2.2, 2.0], hot);
            self.particles.push(Particle {
                x: cx + x_off,
                y,
                vx: dirx / len * speed,
                vy: diry / len * speed,
                life,
                max_life: life,
                size,
                length: 0.0,
                color,
                gravity: GRAVITY,
                style: Style::Spark,
            });
        }

        // 4) Milestone: aerial firework partway up the lane.
        if milestone {
            let burst_y = (y * 0.45).max(60.0);
            for _ in 0..90 {
                if !self.room() {
                    break;
                }
                let ang = self.rand_range(0.0, std::f32::consts::TAU);
                let speed = self.rand_range(160.0, 720.0);
                let life = self.rand_range(0.6, 1.4);
                let size = self.rand_range(6.0, 16.0);
                let hue = self.rand();
                let color = mix(base, [2.2, 2.2, 2.0], hue * 0.8);
                self.particles.push(Particle {
                    x: cx,
                    y: burst_y,
                    vx: ang.cos() * speed,
                    vy: ang.sin() * speed,
                    life,
                    max_life: life,
                    size,
                    length: 0.0,
                    color,
                    gravity: GRAVITY * 0.7,
                    style: Style::Spark,
                });
            }
        }
    }

    /// Note bar struck correctly: light it up. `x`/`w` are the key's logical
    /// x/width; `start_secs`/`dur_secs` come from the matched MIDI note.
    pub fn note_struck(&mut self, note_id: u8, x: f32, w: f32, start_secs: f32, dur_secs: f32) {
        let base = note_linear_color(note_id);
        let life = dur_secs.clamp(0.45, 1.4);
        self.note_flashes.push(NoteFlash {
            x,
            w,
            start: start_secs,
            h: dur_secs.max(0.1) - 0.01,
            life,
            max_life: life,
            color: mix(base, [2.0, 2.0, 1.9], 0.75),
        });
    }

    /// Wrong note: break the combo and drop a small dark puff at the key.
    pub fn wrong_hit(&mut self, cx: f32, y: f32) {
        self.combo = 0;
        self.combo_pop = 0.0;
        self.record_result(false);
        self.total_wrong += 1;

        for _ in 0..14 {
            if !self.room() {
                break;
            }
            let life = self.rand_range(0.35, 0.7);
            let x_off = self.rand_range(-10.0, 10.0);
            let vx = self.rand_range(-110.0, 110.0);
            let vy = self.rand_range(-180.0, -30.0);
            let size = self.rand_range(5.0, 11.0);
            self.particles.push(Particle {
                x: cx + x_off,
                y,
                vx,
                vy,
                life,
                max_life: life,
                size,
                length: 0.0,
                color: [0.4, 0.07, 0.07],
                gravity: GRAVITY * 0.7,
                style: Style::Puff,
            });
        }
    }

    /// Celebration fireworks for the results screen: periodic colourful
    /// bursts at random spots in the upper part of the window.
    pub fn celebrate(&mut self, dt: f32, win_w: f32, win_h: f32) {
        self.celebrate_acc += dt;
        if self.celebrate_acc < 0.55 {
            return;
        }
        self.celebrate_acc = 0.0;

        let x = self.rand_range(win_w * 0.1, win_w * 0.9);
        let y = self.rand_range(win_h * 0.12, win_h * 0.5);

        const NOTES: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
        let note = NOTES[(self.next_u64() % NOTES.len() as u64) as usize];
        let base = note_linear_color(note);

        for _ in 0..70 {
            if !self.room() {
                break;
            }
            let ang = self.rand_range(0.0, std::f32::consts::TAU);
            let speed = self.rand_range(140.0, 640.0);
            let life = self.rand_range(0.6, 1.5);
            let size = self.rand_range(6.0, 15.0);
            let hot = self.rand() * 0.7;
            let color = mix(base, [2.2, 2.2, 2.0], hot);
            self.particles.push(Particle {
                x,
                y,
                vx: ang.cos() * speed,
                vy: ang.sin() * speed,
                life,
                max_life: life,
                size,
                length: 0.0,
                color,
                gravity: GRAVITY * 0.6,
                style: Style::Spark,
            });
        }
    }

    // --- per-frame -----------------------------------------------------------

    pub fn update(&mut self, dt: f32, hit_line_y: f32, board_left: f32, board_width: f32) {
        self.combo_pop = (self.combo_pop - dt * 4.0).max(0.0);

        for f in &mut self.note_flashes {
            f.life -= dt;
        }
        self.note_flashes.retain(|f| f.life > 0.0);

        for r in &mut self.rising {
            r.age += dt;
        }
        self.rising.retain(|r| r.age < r.life);

        // Sweep the sentiment needle toward the rolling accuracy.
        let target = self.accuracy().unwrap_or(0.5);
        self.sentiment_display += (target - self.sentiment_display) * (dt * 3.0).min(1.0);

        // Tall ambient flames while on fire.
        if self.on_fire() {
            let rate = 60.0 + self.combo as f32 * 3.5;
            self.ember_acc += dt * rate;
            while self.ember_acc >= 1.0 {
                self.ember_acc -= 1.0;
                if !self.room() {
                    self.ember_acc = 0.0;
                    break;
                }
                let x = board_left + self.rand() * board_width;
                let life = self.rand_range(0.7, 1.6);
                let y_off = self.rand_range(-3.0, 8.0);
                let vx = self.rand_range(-32.0, 32.0);
                let vy = self.rand_range(-190.0, -70.0);
                let size = self.rand_range(4.0, 9.0);
                let warm = self.rand();
                let color = mix([2.0, 0.55, 0.06], [2.1, 1.5, 0.2], warm);
                self.particles.push(Particle {
                    x,
                    y: hit_line_y + y_off,
                    vx,
                    vy,
                    life,
                    max_life: life,
                    size,
                    length: 0.0,
                    color,
                    gravity: -60.0, // float upward
                    style: Style::Ember,
                });
            }
        } else {
            self.ember_acc = 0.0;
        }

        for p in &mut self.particles {
            p.vy += p.gravity * dt;
            p.x += p.vx * dt;
            p.y += p.vy * dt;
            p.life -= dt;
        }
        self.particles.retain(|p| p.life > 0.0);
    }

    pub fn render(&self, quads: &mut QuadRenderer) {
        for p in &self.particles {
            let t = (p.life / p.max_life).clamp(0.0, 1.0);
            let [r, g, b] = p.color;

            match p.style {
                Style::Beam => {
                    // Capsule of light rising from the hit line, fading out.
                    let w = p.size * (0.6 + 0.4 * t);
                    let h = p.length;
                    let a = t * t * 0.55;
                    let rad = w * 0.5;
                    quads.push(QuadInstance {
                        position: [p.x - w * 0.5, p.y - h],
                        size: [w, h],
                        color: [r, g, b, a],
                        border_radius: [rad, rad, rad, rad],
                    });
                }
                Style::Flash => {
                    // Expanding fading ring/disc.
                    let s = p.size * (1.0 + (1.0 - t) * 1.2);
                    let a = t * 0.8;
                    let half = s * 0.5;
                    quads.push(QuadInstance {
                        position: [p.x - half, p.y - half],
                        size: [s, s],
                        color: [r, g, b, a],
                        border_radius: [half, half, half, half],
                    });
                }
                Style::Spark | Style::Ember => {
                    let core = p.size * (0.35 + 0.65 * t);
                    let core_a = t.powf(0.6);
                    // Soft bloom halo behind the core.
                    let halo = core * 2.8;
                    let halo_a = core_a * 0.22;
                    let hh = halo * 0.5;
                    quads.push(QuadInstance {
                        position: [p.x - hh, p.y - hh],
                        size: [halo, halo],
                        color: [r, g, b, halo_a],
                        border_radius: [hh, hh, hh, hh],
                    });
                    let ch = core * 0.5;
                    quads.push(QuadInstance {
                        position: [p.x - ch, p.y - ch],
                        size: [core, core],
                        color: [r, g, b, core_a],
                        border_radius: [ch, ch, ch, ch],
                    });
                }
                Style::Puff => {
                    let s = p.size * (0.5 + 0.5 * t);
                    let a = t * 0.7;
                    let half = s * 0.5;
                    quads.push(QuadInstance {
                        position: [p.x - half, p.y - half],
                        size: [s, s],
                        color: [r, g, b, a],
                        border_radius: [half, half, half, half],
                    });
                }
            }
        }
    }

    /// Speedometer-style sentiment dial: a semicircular arc of dots running
    /// red (left, terrible) through yellow to green (right, brilliant), with
    /// a needle sweeping to the smoothed audience sentiment.
    pub fn render_sentiment_dial(&self, quads: &mut QuadRenderer, cx: f32, cy: f32, radius: f32) {
        fn gauge_color(t: f32) -> [f32; 3] {
            let red = [0.70, 0.03, 0.03];
            let yellow = [0.75, 0.58, 0.04];
            let green = [0.06, 0.70, 0.10];
            if t < 0.5 {
                mix(red, yellow, t * 2.0)
            } else {
                mix(yellow, green, (t - 0.5) * 2.0)
            }
        }

        // Arc: left (t=0) to right (t=1) over the top half.
        const ARC_DOTS: usize = 24;
        for i in 0..ARC_DOTS {
            let t = i as f32 / (ARC_DOTS - 1) as f32;
            let theta = std::f32::consts::PI * (1.0 - t);
            let x = cx + theta.cos() * radius;
            let y = cy - theta.sin() * radius;
            dot(quads, x, y, 3.0, gauge_color(t), 0.9);
        }

        // Needle.
        let val = self.sentiment_display.clamp(0.0, 1.0);
        let theta = std::f32::consts::PI * (1.0 - val);
        let (sin, cos) = (theta.sin(), theta.cos());
        const NEEDLE_DOTS: usize = 8;
        for i in 0..NEEDLE_DOTS {
            let d = i as f32 / (NEEDLE_DOTS - 1) as f32 * (radius - 6.0);
            dot(quads, cx + cos * d, cy - sin * d, 2.4, [0.9, 0.9, 0.9], 1.0);
        }

        // Hub.
        dot(quads, cx, cy, 7.0, [0.8, 0.8, 0.8], 1.0);
    }

    /// Audience face, drawn entirely from circles (no emoji font needed).
    /// `level` is [`Self::sentiment_level`]: 0 angry .. 5 star-eyed grin.
    pub fn render_sentiment_face(&self, quads: &mut QuadRenderer, cx: f32, cy: f32, level: u8) {
        // Face disc: red when angry, orange when grumpy, classic yellow above.
        let face = match level {
            0 => [0.72, 0.08, 0.05],
            1 => [0.78, 0.32, 0.05],
            _ => [0.82, 0.62, 0.07],
        };
        dot(quads, cx, cy, 40.0, face, 1.0);

        let dark = [0.05, 0.04, 0.03];

        // Eyes: star sparkles at level 5, plain dots otherwise.
        if level == 5 {
            let gold = [1.3, 1.05, 0.25];
            for sx in [-7.0f32, 7.0] {
                let (ex, ey) = (cx + sx, cy - 4.5);
                dot(quads, ex, ey, 5.0, gold, 1.0);
                for (ox, oy) in [(0.0, -4.5), (0.0, 4.5), (-4.5, 0.0), (4.5, 0.0)] {
                    dot(quads, ex + ox, ey + oy, 2.6, gold, 1.0);
                }
            }
        } else {
            for sx in [-7.0f32, 7.0] {
                dot(quads, cx + sx, cy - 4.5, 5.0, dark, 1.0);
            }
        }

        // Angry brows slanting in over the eyes.
        if level == 0 {
            for s in [-1.0f32, 1.0] {
                dot(quads, cx + s * 10.5, cy - 12.5, 2.8, dark, 1.0);
                dot(quads, cx + s * 7.5, cy - 11.0, 2.8, dark, 1.0);
                dot(quads, cx + s * 4.5, cy - 9.5, 2.8, dark, 1.0);
            }
        }

        // Mouth: an arc of dots. Positive `amp` bows the middle down (smile),
        // negative bows it up (frown).
        let (amp, base_y, d) = match level {
            0 => (-4.5, cy + 11.0, 3.2),
            1 => (-3.0, cy + 10.5, 3.0),
            2 => (0.0, cy + 9.0, 3.0),
            3 => (3.0, cy + 7.0, 3.0),
            4 => (4.5, cy + 6.0, 3.6),
            _ => (5.0, cy + 6.0, 3.8),
        };
        const MOUTH_DOTS: usize = 7;
        for i in 0..MOUTH_DOTS {
            let t = i as f32 / (MOUTH_DOTS - 1) as f32;
            let x = cx + (t - 0.5) * 17.0;
            let y = base_y + amp * (1.0 - (2.0 * t - 1.0).powi(2));
            dot(quads, x, y, d, dark, 1.0);
        }
    }

    /// Draw the sheen over correctly-struck note bars. Uses the same geometry
    /// as the waterfall vertex shader: `speed` and `keyboard_y` in logical
    /// coordinates, `time` the same value the waterfall was updated with.
    pub fn render_note_flashes(
        &self,
        quads: &mut QuadRenderer,
        time: f32,
        speed: f32,
        keyboard_y: f32,
    ) {
        for f in &self.note_flashes {
            let t = (f.life / f.max_life).clamp(0.0, 1.0);

            let bar_h = f.h * speed.abs();
            let mut y = keyboard_y;
            if speed > 0.0 {
                y -= bar_h;
            }
            y -= (f.start - time) * speed;

            // Clip to the lane so the sheen never covers the keyboard.
            let bottom = (y + bar_h).min(keyboard_y);
            let h_vis = bottom - y;
            if h_vis <= 1.0 {
                continue;
            }

            // Bright pop that settles into a gentle shimmer while the bar
            // finishes crossing the line.
            let age = f.max_life - f.life;
            let shimmer = 0.5 + 0.5 * (age * 16.0).sin();
            let a = t.powf(0.7) * (0.38 + 0.18 * shimmer);

            let r = (f.w * 0.2).min(h_vis * 0.5);
            quads.push(QuadInstance {
                position: [f.x, y],
                size: [f.w, h_vis],
                color: [f.color[0], f.color[1], f.color[2], a],
                border_radius: [r, r, r, r],
            });
        }
    }
}

/// Rainbow palette matching the white-key squares, in linear RGB.
/// Sharps (black keys) spark icy white.
fn note_linear_color(note_id: u8) -> [f32; 3] {
    let (r, g, b) = match note_id % 12 {
        0 => (230, 30, 30),    // C - Red
        2 => (255, 140, 0),    // D - Orange
        4 => (255, 215, 0),    // E - Yellow
        5 => (40, 190, 60),    // F - Green
        7 => (40, 90, 230),    // G - Blue
        9 => (150, 60, 210),   // A - Purple
        11 => (255, 105, 180), // B - Pink
        _ => (210, 235, 255),  // sharps - icy white
    };
    [s2l(r), s2l(g), s2l(b)]
}

fn s2l(c: u8) -> f32 {
    let u = c as f32 / 255.0;
    if u < 0.04045 {
        u / 12.92
    } else {
        ((u + 0.055) / 1.055).powf(2.4)
    }
}

/// Push a filled circle of diameter `d` centred at (x, y).
fn dot(quads: &mut QuadRenderer, x: f32, y: f32, d: f32, color: [f32; 3], a: f32) {
    let half = d * 0.5;
    quads.push(QuadInstance {
        position: [x - half, y - half],
        size: [d, d],
        color: [color[0], color[1], color[2], a],
        border_radius: [half, half, half, half],
    });
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}
