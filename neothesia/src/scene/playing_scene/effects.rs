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

use neothesia_core::render::{QuadInstance, QuadRenderer};

const GRAVITY: f32 = 1350.0;
const MAX_PARTICLES: usize = 6000;
const FIRE_COMBO: u32 = 15;

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

pub struct EffectsSystem {
    particles: Vec<Particle>,
    rng: u64,

    combo: u32,
    best_combo: u32,
    combo_pop: f32,
    ember_acc: f32,

    /// Full-screen flash timer (milestones / wrong notes) and its colour.
    screen_flash: f32,
    screen_flash_color: [f32; 3],
}

impl EffectsSystem {
    pub fn new() -> Self {
        Self {
            particles: Vec::new(),
            rng: 0x9E3779B97F4A7C15,
            combo: 0,
            best_combo: 0,
            combo_pop: 0.0,
            ember_acc: 0.0,
            screen_flash: 0.0,
            screen_flash_color: [1.0, 1.0, 1.0],
        }
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

    // --- event hooks ---------------------------------------------------------

    /// Correct note. `cx` = key centre, `y` = hit line (keyboard top, which is
    /// also the height of the note lane above it), `key_w` = white-key width.
    pub fn good_hit(&mut self, note_id: u8, cx: f32, y: f32, key_w: f32) {
        self.combo += 1;
        self.best_combo = self.best_combo.max(self.combo);
        self.combo_pop = 1.0;

        let mult = self.multiplier() as f32;
        let base = note_linear_color(note_id);
        let milestone = self.combo % 10 == 0;
        let fire = self.on_fire();

        // 1) Light pillar up the note lane.
        if self.room() {
            let lane = (y * 0.92).clamp(120.0, 1100.0);
            self.particles.push(Particle {
                x: cx,
                y,
                vx: 0.0,
                vy: 0.0,
                life: if milestone { 0.5 } else { 0.36 },
                max_life: if milestone { 0.5 } else { 0.36 },
                size: key_w * if milestone { 1.5 } else { 1.05 },
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

        // 4) Milestone: aerial firework partway up the lane + screen flash.
        if milestone {
            self.screen_flash = 1.0;
            self.screen_flash_color = if fire { [1.9, 0.7, 0.15] } else { [1.4, 1.5, 2.0] };

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

    /// Wrong note: break the combo, grey puff, brief red screen flash.
    pub fn wrong_hit(&mut self, cx: f32, y: f32) {
        self.combo = 0;
        self.combo_pop = 0.0;
        self.screen_flash = self.screen_flash.max(0.5);
        self.screen_flash_color = [0.7, 0.05, 0.05];

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

    // --- per-frame -----------------------------------------------------------

    pub fn update(&mut self, dt: f32, hit_line_y: f32, board_left: f32, board_width: f32) {
        self.combo_pop = (self.combo_pop - dt * 4.0).max(0.0);
        self.screen_flash = (self.screen_flash - dt * 3.2).max(0.0);

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

    /// Full-screen colour flash for milestones / wrong notes.
    pub fn render_screen_flash(&self, quads: &mut QuadRenderer, win_w: f32, win_h: f32) {
        if self.screen_flash <= 0.001 {
            return;
        }
        let [r, g, b] = self.screen_flash_color;
        let a = self.screen_flash * self.screen_flash * 0.28;
        quads.push(QuadInstance {
            position: [0.0, 0.0],
            size: [win_w, win_h],
            color: [r, g, b, a],
            border_radius: [0.0; 4],
        });
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

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}
