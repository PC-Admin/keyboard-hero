//! Guitar-Hero-style visual effects for play-along mode.
//!
//! When the player hits the right key at the right time we emit a burst of
//! sparks from the hit line, keep a running combo/multiplier counter, and set
//! the keyboard "on fire" (ambient rising embers) once the streak gets long.
//! A wrong note drops a little grey puff and breaks the combo.
//!
//! Everything is drawn as plain rounded quads through the existing foreground
//! [`QuadRenderer`], so there is no extra GPU pipeline to manage. Nothing here
//! triggers unless a track is set to "Human" (i.e. you are playing along), so
//! ordinary auto-play is completely unaffected.

use neothesia_core::render::{QuadInstance, QuadRenderer};

const GRAVITY: f32 = 1600.0;
/// Hard cap so a mashed keyboard can never blow up memory / draw calls.
const MAX_PARTICLES: usize = 3200;
/// Combo length at which the keyboard catches fire.
const FIRE_COMBO: u32 = 15;

struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    /// Seconds of life remaining.
    life: f32,
    max_life: f32,
    size: f32,
    /// Linear RGB.
    color: [f32; 3],
    /// Downward acceleration (negative for embers that float up).
    gravity: f32,
    /// Flash rings expand and fade instead of shrinking like sparks.
    flash: bool,
}

pub struct EffectsSystem {
    particles: Vec<Particle>,
    rng: u64,

    combo: u32,
    best_combo: u32,
    /// 1.0 right after a hit, decays to 0 — drives the combo label pulse.
    combo_pop: f32,
    /// Fractional accumulator for ambient ember emission.
    ember_acc: f32,
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
        }
    }

    // --- tiny xorshift PRNG (no external deps) ------------------------------

    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Uniform in [0, 1).
    fn rand(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn rand_range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.rand()
    }

    // --- public state for the HUD ------------------------------------------

    pub fn combo(&self) -> u32 {
        self.combo
    }

    pub fn best_combo(&self) -> u32 {
        self.best_combo
    }

    /// Score multiplier, Guitar-Hero style: x1..x4 as the combo grows.
    pub fn multiplier(&self) -> u32 {
        (1 + self.combo / 10).min(4)
    }

    /// 0.0..1.0 pulse value used to briefly enlarge the combo label on a hit.
    pub fn combo_pop(&self) -> f32 {
        self.combo_pop
    }

    pub fn on_fire(&self) -> bool {
        self.combo >= FIRE_COMBO
    }

    // --- event hooks --------------------------------------------------------

    /// Correct note played. `cx` is the key centre, `y` the hit line (keyboard
    /// top), `key_w` the white-key width used to scale the flash.
    pub fn good_hit(&mut self, note_id: u8, cx: f32, y: f32, key_w: f32) {
        self.combo += 1;
        self.best_combo = self.best_combo.max(self.combo);
        self.combo_pop = 1.0;

        let mult = self.multiplier() as f32;
        let base = note_linear_color(note_id);

        // Milestone every 10 hits gets an extra-big pop.
        let milestone = self.combo % 10 == 0;
        let count = (14.0 + mult * 6.0 + if milestone { 22.0 } else { 0.0 }) as usize;

        for _ in 0..count {
            if self.particles.len() >= MAX_PARTICLES {
                break;
            }
            let speed = self.rand_range(190.0, 640.0) * (0.85 + 0.08 * mult);
            let dirx = self.rand_range(-1.0, 1.0);
            let diry = self.rand_range(-1.0, -0.28); // bias upward (screen -y)
            let len = (dirx * dirx + diry * diry).sqrt().max(0.0001);
            let life = self.rand_range(0.35, 0.9);
            let x_off = self.rand_range(-key_w * 0.2, key_w * 0.2);
            let size = self.rand_range(2.5, 6.5);

            // Hot white core mixed toward the note colour.
            let hot = self.rand() * 0.65;
            let color = mix(base, [1.6, 1.6, 1.5], hot);

            self.particles.push(Particle {
                x: cx + x_off,
                y,
                vx: dirx / len * speed,
                vy: diry / len * speed,
                life,
                max_life: life,
                size,
                color,
                gravity: GRAVITY,
                flash: false,
            });
        }

        // Bright flash ring at the key.
        if self.particles.len() < MAX_PARTICLES {
            let flash_col = mix(base, [1.8, 1.8, 1.7], 0.55);
            self.particles.push(Particle {
                x: cx,
                y,
                vx: 0.0,
                vy: 0.0,
                life: if milestone { 0.28 } else { 0.18 },
                max_life: if milestone { 0.28 } else { 0.18 },
                size: key_w * if milestone { 2.4 } else { 1.6 },
                color: flash_col,
                gravity: 0.0,
                flash: true,
            });
        }
    }

    /// Wrong note: break the combo and drop a small grey puff.
    pub fn wrong_hit(&mut self, cx: f32, y: f32) {
        self.combo = 0;
        self.combo_pop = 0.0;

        for _ in 0..8 {
            if self.particles.len() >= MAX_PARTICLES {
                break;
            }
            let life = self.rand_range(0.3, 0.6);
            let x_off = self.rand_range(-6.0, 6.0);
            let vx = self.rand_range(-70.0, 70.0);
            let vy = self.rand_range(-120.0, -20.0);
            let size = self.rand_range(2.0, 4.5);
            self.particles.push(Particle {
                x: cx + x_off,
                y,
                vx,
                vy,
                life,
                max_life: life,
                size,
                color: [0.35, 0.06, 0.06],
                gravity: GRAVITY * 0.6,
                flash: false,
            });
        }
    }

    // --- per-frame ----------------------------------------------------------

    pub fn update(&mut self, dt: f32, hit_line_y: f32, board_left: f32, board_width: f32) {
        // Decay the combo label pulse.
        self.combo_pop = (self.combo_pop - dt * 4.0).max(0.0);

        // Ambient rising embers while "on fire".
        if self.on_fire() {
            let rate = 26.0 + self.combo as f32 * 2.0;
            self.ember_acc += dt * rate;
            while self.ember_acc >= 1.0 {
                self.ember_acc -= 1.0;
                if self.particles.len() >= MAX_PARTICLES {
                    self.ember_acc = 0.0;
                    break;
                }
                let x = board_left + self.rand() * board_width;
                let life = self.rand_range(0.5, 1.15);
                let y_off = self.rand_range(-2.0, 6.0);
                let vx = self.rand_range(-22.0, 22.0);
                let vy = self.rand_range(-90.0, -34.0);
                let size = self.rand_range(1.5, 3.6);
                // Warm orange/yellow embers.
                let warm = self.rand();
                let color = mix([1.6, 0.5, 0.08], [1.7, 1.2, 0.15], warm);
                self.particles.push(Particle {
                    x,
                    y: hit_line_y + y_off,
                    vx,
                    vy,
                    life,
                    max_life: life,
                    size,
                    color,
                    gravity: -45.0, // float upward
                    flash: false,
                });
            }
        } else {
            self.ember_acc = 0.0;
        }

        // Integrate & cull.
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

            let (size, alpha) = if p.flash {
                // Expand and fade.
                (p.size * (1.0 + (1.0 - t) * 0.9), t * 0.7)
            } else {
                // Shrink and fade (ease-out on alpha for a snappier tail).
                (p.size * (0.3 + 0.7 * t), t.powf(0.7))
            };

            let half = size * 0.5;
            quads.push(QuadInstance {
                position: [p.x - half, p.y - half],
                size: [size, size],
                color: [p.color[0], p.color[1], p.color[2], alpha],
                border_radius: [half, half, half, half], // circle
            });
        }
    }
}

/// Rainbow palette matching the white-key squares, converted to linear RGB.
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

/// sRGB 8-bit component to linear float.
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
