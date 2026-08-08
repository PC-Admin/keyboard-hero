//! Guitar-Hero-style visual effects for play-along mode — the "dream big" edition.
//!
//! On a correct hit we blast a **light pillar** up the note lane, throw a fat
//! burst of glowing sparks (each with a soft bloom halo), pop a big flash ring
//! on the key, and bump a running combo / x1..x4 multiplier — once per chord,
//! not once per key, since a chord is a single musical event. Every 10 hits
//! fires a full milestone explosion + screen flash. Pass a 15 streak and the
//! keyboard is "on fire" — tall ambient flames rise from the hit line. A wrong
//! note breaks the combo with a grey puff, and so does a note the song had to
//! stall and wait for (wait-mode catch-ups earn "TOO SLOW", not streak).
//!
//! A chain of clean PERFECT chords ([`BOLT_CHORDS`] of them) calls **lightning**
//! down: a bolt rakes in across the lane onto the key that earned it, and the
//! strike leaves the board surging for [`SURGE_SECS`] — keyboard, falling bars
//! and the score readout all lit electric, every note worth +50%. The chain
//! does not build while the board is already surging: the lights have to go
//! down before another one can be earned.
//!
//! Everything draws as plain rounded quads through the existing foreground
//! [`QuadRenderer`] (bloom faked with additive-ish translucent halos over the
//! dark background), so there is no extra GPU pipeline. With a Human track the
//! player is graded in wait mode; with none, jam mode grades whatever they
//! play over the self-playing song — unplayed notes are silent misses.

use std::{collections::VecDeque, time::Instant};

use neothesia_core::render::{QuadInstance, QuadRenderer};

const GRAVITY: f32 = 1350.0;
const MAX_PARTICLES: usize = 6000;
const FIRE_COMBO: u32 = 15;
/// How many recent notes the "audience" judges you on.
const SENTIMENT_WINDOW: usize = 20;
/// Hit-timing grades (seconds between the file note and your press). Wide
/// enough that a human sight-reading on a real keyboard can land PERFECTs:
/// MIDI/audio latency alone eats tens of ms before your playing is judged at
/// all. They still separate "in time" from "roughly the right note
/// eventually" — a note only stops being GOOD once it is a noticeable beat
/// behind.
const PERFECT_WINDOW: f32 = 0.12;
const GOOD_WINDOW: f32 = 0.28;
/// Wait-mode only: how long the song may sit stalled on a note before the
/// eventual catch-up stops counting as playing it and becomes "TOO SLOW"
/// (combo break, near-zero credit). Reaction time to a note you did not see
/// coming is a few hundred ms, so the cutoff has to sit above that.
const SLOW_WINDOW: f32 = 0.45;
/// Notes the song starts within this of each other are one chord, and so are
/// worth one combo step between them. Comfortably under a 32nd note at 120bpm
/// (~62ms), so a fast run still counts note by note; wide enough to absorb a
/// chord the file itself voices slightly spread.
const CHORD_WINDOW: std::time::Duration = std::time::Duration::from_millis(30);
/// Consecutive chords struck entirely on PERFECT timing that call down the
/// lightning. Chords, not keys, so a phrase nailed dead-on earns it whether
/// those chords are single notes or fistfuls.
///
/// TEMPORARY: dialled down to 2 so the effect is easy to trigger by hand.
/// Put it back to 5 for real play.
pub const BOLT_CHORDS: u32 = 2;
/// How long the board stays lit after a bolt lands. Long enough to be worth
/// pushing for, short enough that it has to be re-earned.
const SURGE_SECS: f32 = 8.0;
/// Score boost while surging, as a fraction: +50% per note.
const SURGE_NUM: u64 = 3;
const SURGE_DEN: u64 = 2;
/// The surge's palette, in linear RGB.
///
/// Broad washes have to be *saturated and under 1.0*: a channel over 1.0
/// clamps, and a translucent film of clamped white-blue just greys out
/// whatever is beneath it (the keyboard especially). Small hot details —
/// the bolt core, the filament along the hit line — do want to blow out,
/// so they get their own colour above 1.0.
const ARC_TINT: [f32; 3] = [0.05, 0.35, 1.0];
const ARC_HOT: [f32; 3] = [1.5, 2.0, 2.4];
/// Middle ground for the sparks: blue, but bright enough to read as a spark.
const ARC_SPARK: [f32; 3] = [0.25, 0.85, 2.1];

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

/// A lightning bolt cracking down the lane onto a struck key.
///
/// The jagged path is baked once in screen coordinates — the strike is over in
/// a few frames, so it has no reason to track anything that moves — and drawn
/// as a chain of glowing dots, since the quad renderer cannot rotate a bar to
/// lie along a diagonal.
struct Bolt {
    /// Main channel, top of the lane down to the key. Last point is the key.
    path: Vec<[f32; 2]>,
    /// Short branches hanging off the channel, for the forked look.
    forks: Vec<Vec<[f32; 2]>>,
    /// Core thickness at the top; the bolt tapers as it descends.
    width: f32,
    life: f32,
    max_life: f32,
}

/// Timing tag on a rising text: how the note was struck.
#[derive(Clone, Copy, PartialEq)]
pub enum RisingGrade {
    Perfect,
    Good,
    /// Correct key, but only after the song stalled and waited for it.
    Slow,
}

/// "PERFECT!" / "GOOD" / "TOO SLOW" text rising out of the top of a struck key.
pub struct RisingText {
    x: f32,
    /// Key top (hit line) — the text rises up from here.
    y0: f32,
    age: f32,
    life: f32,
    pub grade: RisingGrade,
}

impl RisingText {
    pub fn text(&self) -> &'static str {
        match self.grade {
            RisingGrade::Perfect => "PERFECT!",
            RisingGrade::Good => "GOOD",
            RisingGrade::Slow => "TOO SLOW",
        }
    }

    pub fn x(&self) -> f32 {
        self.x
    }

    pub fn y(&self) -> f32 {
        let rise = match self.grade {
            RisingGrade::Perfect => 110.0,
            RisingGrade::Good => 80.0,
            RisingGrade::Slow => 55.0,
        };
        self.y0 - 24.0 - self.age * rise
    }

    pub fn alpha(&self) -> f32 {
        (1.0 - self.age / self.life).clamp(0.0, 1.0).powf(0.8)
    }

    pub fn font_size(&self) -> f32 {
        let base = if self.grade == RisingGrade::Perfect {
            24.0
        } else {
            19.0
        };
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

    /// Song-time of the chord the last correct hit belonged to, so the notes
    /// of one chord advance the combo once between them.
    last_chord: Option<Instant>,

    /// Consecutive chords struck entirely on PERFECT timing. Reaching
    /// [`BOLT_CHORDS`] calls the lightning down and empties the count; it stays
    /// empty for as long as the resulting surge lasts.
    perfect_chords: u32,
    /// Is the chord being played right now still all-PERFECT? One late key
    /// spoils it, and the streak with it.
    chord_all_perfect: bool,

    /// Lightning currently in flight.
    bolts: Vec<Bolt>,
    /// White-out from a bolt landing; decays over a few frames.
    strike_flash: f32,
    /// Seconds left on the surge a bolt kicks off; 0 when not surging. A wrong
    /// note does not cut it short — the window is earned time, and the streak
    /// it came from is already gone.
    surge: f32,
    /// Ever-rising clock driving the surge's pulsing glow.
    surge_phase: f32,
    /// Fractional arc particles owed along the hit line while surging.
    arc_acc: f32,
    /// Keyboard extent, remembered from the last frame so a bolt can be aimed
    /// in from off the side of the board. Zero until the first update.
    board_left: f32,
    board_width: f32,

    /// Smoothed 0..1 sentiment shown by the dial needle (eases toward the
    /// rolling accuracy so it sweeps like a real gauge).
    sentiment_display: f32,

    // Whole-song tallies for the results screen.
    total_perfect: u32,
    total_good: u32,
    total_ok: u32,
    total_wrong: u32,
    /// Jam-mode targets nobody played (silent combo breaks).
    total_missed: u32,

    /// Arcade score: timing points per note, boosted by the combo multiplier.
    score: u64,

    /// Timer for celebration fireworks on the results screen.
    celebrate_acc: f32,
}

/// Points per hit, before the combo multiplier.
const PTS_PERFECT: u64 = 100;
const PTS_GOOD: u64 = 60;
const PTS_SLOW: u64 = 20;

/// Credit each kind of note earns toward the performance score, and what a
/// mistake costs. A GOOD is worth nearly a PERFECT — landing the note is the
/// hard part — while a note the song had to stall and wait for is worth
/// little, and a note nobody played at all only counts half against you.
const W_GOOD: f32 = 0.80;
const W_OK: f32 = 0.35;
const W_WRONG_PENALTY: f32 = 0.40;
const MISS_WEIGHT: f32 = 0.5;

/// Whole-song performance summary for the results screen.
#[derive(Clone, Copy)]
pub struct Results {
    pub perfect: u32,
    pub good: u32,
    pub ok: u32,
    pub wrong: u32,
    pub missed: u32,
    pub best_combo: u32,
    pub score: u64,
}

impl Results {
    pub fn total_hit(&self) -> u32 {
        self.perfect + self.good + self.ok
    }

    pub fn accuracy(&self) -> f32 {
        let total = self.total_hit() + self.wrong + self.missed;
        if total == 0 {
            return 0.0;
        }
        self.total_hit() as f32 / total as f32
    }

    /// Timing-weighted performance score in 0..1. Accuracy alone is too easy
    /// in wait-mode (the song waits for you, so avoiding wrong notes is most
    /// of it) — the grade should still reward *precision*: a PERFECT is full
    /// credit, a GOOD nearly all of it, and a hit the song had to stall and
    /// wait for earns little. Wrong notes cost more than the note they
    /// occupy, so flailing can't be papered over by volume of right notes.
    ///
    /// Notes nobody played count for *half* a note against you rather than a
    /// whole one: in the jam modes the song is playing itself and you are
    /// playing over it, so sitting out a phrase should cost less than
    /// fumbling it.
    pub fn performance(&self) -> f32 {
        let total = self.total_hit() as f32 + self.wrong as f32 + self.missed as f32 * MISS_WEIGHT;
        if total <= 0.0 {
            return 0.0;
        }
        // A miss is a zero-credit attempt; a wrong note costs extra on top.
        let weighted =
            self.perfect as f32 * 1.0 + self.good as f32 * W_GOOD + self.ok as f32 * W_OK;
        let penalty = self.wrong as f32 * W_WRONG_PENALTY;
        ((weighted - penalty) / total).clamp(0.0, 1.0)
    }

    /// Arcade letter grade with its display colour, from the performance
    /// score. Fine-grained ladder from A++ (flawless, gold) down to F. An A
    /// still has to be earned on timing, but the ladder is pitched so that
    /// playing a song *well* lands in the A/B range rather than the C's:
    /// clean all-GOOD play is an A-, a competent run with a few fumbles is a
    /// B, and only a run the song spent its whole time waiting for lands in
    /// the D's.
    pub fn grade(&self) -> (&'static str, (u8, u8, u8)) {
        const GOLD: (u8, u8, u8) = (255, 200, 40);
        const GREEN: (u8, u8, u8) = (80, 220, 90);
        const BLUE: (u8, u8, u8) = (70, 140, 255);
        const ORANGE: (u8, u8, u8) = (255, 150, 50);
        const RED_ORANGE: (u8, u8, u8) = (235, 95, 50);
        const RED: (u8, u8, u8) = (225, 55, 50);

        const LADDER: [(f32, &str, (u8, u8, u8)); 12] = [
            (0.96, "A++", GOLD),
            (0.90, "A+", GREEN),
            (0.84, "A", GREEN),
            (0.78, "A-", GREEN),
            (0.72, "B+", BLUE),
            (0.65, "B", BLUE),
            (0.58, "B-", BLUE),
            (0.51, "C+", ORANGE),
            (0.44, "C", ORANGE),
            (0.37, "C-", ORANGE),
            (0.29, "D+", RED_ORANGE),
            (0.21, "D", RED_ORANGE),
        ];

        let score = self.performance();
        for (min, name, color) in LADDER {
            if score >= min {
                return (name, color);
            }
        }
        ("F", RED)
    }

    /// Does this performance deserve fireworks? (A- or better.)
    pub fn celebratory(&self) -> bool {
        self.performance() >= 0.78
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
            last_chord: None,
            perfect_chords: 0,
            chord_all_perfect: false,
            bolts: Vec::new(),
            strike_flash: 0.0,
            surge: 0.0,
            surge_phase: 0.0,
            arc_acc: 0.0,
            board_left: 0.0,
            board_width: 0.0,
            sentiment_display: 0.5,
            total_perfect: 0,
            total_good: 0,
            total_ok: 0,
            total_wrong: 0,
            total_missed: 0,
            score: 0,
            celebrate_acc: 0.0,
        }
    }

    pub fn results(&self) -> Results {
        Results {
            perfect: self.total_perfect,
            good: self.total_good,
            ok: self.total_ok,
            wrong: self.total_wrong,
            missed: self.total_missed,
            best_combo: self.best_combo,
            score: self.score,
        }
    }

    pub fn score(&self) -> u64 {
        self.score
    }

    /// Was this note struck as part of the same chord as the previous one?
    /// Compares when the *song* asked for the two notes, not when they were
    /// played, so a chord rolled by the user still reads as one chord.
    fn is_same_chord(&self, chord: Option<Instant>) -> bool {
        let (Some(chord), Some(last)) = (chord, self.last_chord) else {
            return false;
        };
        let gap = if chord >= last {
            chord - last
        } else {
            last - chord
        };
        gap <= CHORD_WINDOW
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

    /// Is the board still surging from a lightning strike?
    pub fn surging(&self) -> bool {
        self.surge > 0.0
    }

    /// Whole seconds left on the surge, for the HUD countdown.
    pub fn surge_secs_left(&self) -> u32 {
        self.surge.ceil().max(0.0) as u32
    }

    /// Surge brightness 0..1: full while it runs, dimming over the last
    /// second so the lights fade out instead of snapping off.
    pub fn surge_intensity(&self) -> f32 {
        (self.surge / 1.0).clamp(0.0, 1.0)
    }

    /// 0..1 pulse the surge glow breathes on.
    pub fn surge_pulse(&self) -> f32 {
        0.5 + 0.5 * (self.surge_phase * 7.5).sin()
    }

    /// Consecutive all-PERFECT chords banked so far.
    pub fn perfect_chords(&self) -> u32 {
        self.perfect_chords
    }

    /// Points a note is worth right now, after the combo multiplier and the
    /// surge bonus. Integer maths throughout, so the displayed score is exact.
    fn apply_multipliers(&self, pts: u64) -> u64 {
        let pts = pts * self.multiplier() as u64;
        if self.surging() {
            pts * SURGE_NUM / SURGE_DEN
        } else {
            pts
        }
    }

    /// Any mistake — wrong key, missed note, a note the song waited for, or
    /// merely a GOOD instead of a PERFECT — puts the bolt back out of reach.
    fn break_perfect_streak(&mut self) {
        self.perfect_chords = 0;
        self.chord_all_perfect = false;
    }

    /// True once the player has actually played something this song.
    /// Has the *user* done anything gradeable? Misses don't count — an
    /// unattended jam-mode run must not end in a results screen (and a
    /// booing crowd) when nobody was playing.
    pub fn has_activity(&self) -> bool {
        self.total_perfect + self.total_good + self.total_ok + self.total_wrong > 0
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

        Some(if accuracy >= 0.88 && self.recent.len() >= 10 {
            5
        } else if accuracy >= 0.75 {
            4
        } else if accuracy >= 0.60 {
            3
        } else if accuracy >= 0.42 {
            2
        } else if accuracy >= 0.25 {
            1
        } else {
            0
        })
    }

    // --- event hooks ---------------------------------------------------------

    /// Correct note. `cx` = key centre, `y` = hit line (keyboard top, which is
    /// also the height of the note lane above it), `key_w` = white-key width,
    /// `delta_secs` = timing gap between the file note and the press, `late` =
    /// the press came after the file note (wait-mode stalled for it), `chord` =
    /// when the song asked for the note, shared by every note of a chord.
    pub fn good_hit(
        &mut self,
        note_id: u8,
        cx: f32,
        y: f32,
        key_w: f32,
        delta_secs: f32,
        late: bool,
        chord: Option<Instant>,
    ) {
        // A chord is one musical event, so it is worth one combo step however
        // many keys it puts down. Sparks, flashes and per-note timing tallies
        // still fire per key — only the streak counts the chord once.
        let first_in_chord = !self.is_same_chord(chord);
        self.last_chord = chord;

        // The song sat there waiting for this key long enough that it read as
        // a stall, not as playing — right note, but no combo credit and the
        // audience is not impressed. A merely-behind catch-up inside
        // [`SLOW_WINDOW`] keeps its streak; only real dawdling breaks it.
        if late && delta_secs > SLOW_WINDOW {
            self.combo = 0;
            self.combo_pop = 0.0;
            self.break_perfect_streak();
            self.record_result(false);
            self.total_ok += 1;
            // Flat consolation points: the combo just reset, so no multiplier.
            // A surge already burning still pays out on it, though.
            self.score += if self.surging() {
                PTS_SLOW * SURGE_NUM / SURGE_DEN
            } else {
                PTS_SLOW
            };
            self.rising.push(RisingText {
                x: cx,
                y0: y,
                age: 0.0,
                life: 0.9,
                grade: RisingGrade::Slow,
            });
            self.puff(cx, y, [0.28, 0.28, 0.32]);
            return;
        }

        if first_in_chord {
            self.combo += 1;
            self.best_combo = self.best_combo.max(self.combo);
            self.combo_pop = 1.0;
        }
        self.record_result(true);

        let perfect = delta_secs <= PERFECT_WINDOW;
        let pts = if perfect {
            self.total_perfect += 1;
            PTS_PERFECT
        } else if delta_secs <= GOOD_WINDOW {
            self.total_good += 1;
            PTS_GOOD
        } else {
            self.total_ok += 1;
            PTS_SLOW
        };

        // --- PERFECT chord chain -> lightning --------------------------------
        // Nothing charges while the board is already surging: the reward has to
        // run out, and then be earned again from nothing. Counting through the
        // surge would have a chain banked the moment it expired and re-strike
        // instantly, so the lights would never actually go down.
        if !self.surging() {
            // A chord counts once, and only if *every* key in it landed
            // PERFECT: the first key opens the chord's account, a later sloppy
            // one spoils both the chord and the chain it was building.
            if first_in_chord {
                self.chord_all_perfect = perfect;
                if perfect {
                    self.perfect_chords += 1;
                } else {
                    self.perfect_chords = 0;
                }
            } else if !perfect && self.chord_all_perfect {
                self.break_perfect_streak();
            }

            if self.perfect_chords >= BOLT_CHORDS {
                // Struck, on the key that completed the chain — a chord
                // finishing the chain is judged on that key alone, because the
                // bolt has to land while the hit is still on screen and there
                // is no later moment at which a chord is known to be over.
                self.perfect_chords = 0;
                self.surge = SURGE_SECS;
                self.strike_flash = 1.0;
                self.spawn_bolt(cx, y, key_w);
            }
        }

        // Scored after the strike, so the note that summoned the bolt is the
        // first one paid at the surge rate.
        self.score += self.apply_multipliers(pts);

        // Timing grade text rising out of the key.
        if delta_secs <= GOOD_WINDOW {
            self.rising.push(RisingText {
                x: cx,
                y0: y,
                age: 0.0,
                life: 0.9,
                grade: if delta_secs <= PERFECT_WINDOW {
                    RisingGrade::Perfect
                } else {
                    RisingGrade::Good
                },
            });
        }

        let mult = self.multiplier() as f32;
        let base = note_linear_color(note_id);
        // Only the note that actually advanced the combo can trip a milestone,
        // or a chord landing on one would fire the explosion once per key.
        let milestone = first_in_chord && self.combo % 10 == 0;

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
        self.break_perfect_streak();
        self.record_result(false);
        self.total_wrong += 1;
        self.puff(cx, y, [0.4, 0.07, 0.07]);
    }

    /// Jam-mode miss: the song asked, nobody answered. Breaks the combo
    /// with no sound, no puff, no text — failure by omission is quiet.
    pub fn miss(&mut self) {
        self.combo = 0;
        self.combo_pop = 0.0;
        self.break_perfect_streak();
        self.record_result(false);
        self.total_missed += 1;
    }

    /// Crack a bolt from the top of the note lane down onto the key at `cx`,
    /// `y` being the hit line. The path wanders while it is high up and
    /// converges on the key as it descends, so it unmistakably *lands* there.
    fn spawn_bolt(&mut self, cx: f32, y: f32, key_w: f32) {
        const STEPS: usize = 18;

        // Come in low and hard from one side instead of dropping straight
        // down: a channel that rakes across the lane at a shallow angle reads
        // far more violent, and it sweeps over the falling bars on its way in.
        // Whichever side leaves the longer run gets it, so the angle stays
        // shallow wherever on the keyboard the key sits.
        let span = if self.board_width > 0.0 {
            self.board_width
        } else {
            key_w * 52.0
        };
        let left = self.board_left;
        let from_left = cx - left > span * 0.5;
        let entry = if from_left {
            left - span * 0.10
        } else {
            left + span * 1.10
        };
        // Enters high in the lane, but the long horizontal run is what makes
        // the angle: a couple of screen-widths sideways to one lane-height down.
        let entry_y = y * self.rand_range(0.02, 0.20);

        let (dx, dy) = (cx - entry, y - entry_y);
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        // Kinks go across the channel, not sideways in screen space, so the
        // zigzag looks the same however shallow the approach is.
        let (px, py) = (-dy / len, dx / len);
        let amp = key_w * 2.2;

        let mut path = Vec::with_capacity(STEPS + 1);
        for i in 0..=STEPS {
            let t = i as f32 / STEPS as f32;
            // Slack left in the path here: plenty on the way in, none at all
            // at the key, so the bolt unmistakably lands on it.
            let slack = (1.0 - t).powf(0.8);
            // Kinks alternate side to side. Left to pure chance the channel
            // wanders in gentle curves and reads like a line graph; forcing the
            // reversal is what makes it look struck.
            let side = if i % 2 == 0 { 1.0 } else { -1.0 };
            let kink = side * self.rand_range(0.35, 1.0) * amp * slack;
            // Shift each kink a little along the channel too, so the segments
            // come out uneven rather than as a metronomic sawtooth. The end
            // points stay put: the last one *is* the key being struck.
            let jitter = self.rand_range(-0.4, 0.4) / STEPS as f32;
            let along = if i == 0 || i == STEPS { t } else { t + jitter };
            path.push([
                entry + dx * along + px * kink,
                entry_y + dy * along + py * kink,
            ]);
        }

        // A couple of dead-end branches, veering off the channel but still
        // travelling roughly the way the bolt is going.
        let mut forks = Vec::new();
        for _ in 0..2 {
            let from = 3 + (self.next_u64() % (STEPS as u64 - 8)) as usize;
            let [ox, oy] = path[from];
            let side = if self.rand() < 0.5 { -1.0 } else { 1.0 };
            let mut fork = vec![[ox, oy]];
            let (mut bx, mut by) = (ox, oy);
            let steps = self.rand_range(2.0, 5.0) as usize;
            for _ in 0..steps {
                let step = self.rand_range(26.0, 70.0);
                let spread = self.rand_range(0.35, 0.9) * side;
                bx += (dx / len + px * spread) * step;
                by += (dy / len + py * spread) * step;
                fork.push([bx, by]);
            }
            forks.push(fork);
        }

        let width = self.rand_range(8.0, 11.0);
        self.bolts.push(Bolt {
            path,
            forks,
            width,
            life: 0.34,
            max_life: 0.34,
        });

        // The impact: a flash ring on the key and a spray of sparks skidding on
        // in the direction the bolt was travelling. Deliberately modest — the
        // bolt is the spectacle, and a big burst here only smothers the lane.
        if self.room() {
            self.particles.push(Particle {
                x: cx,
                y,
                vx: 0.0,
                vy: 0.0,
                life: 0.26,
                max_life: 0.26,
                size: key_w * 2.4,
                length: 0.0,
                color: ARC_HOT,
                gravity: 0.0,
                style: Style::Flash,
            });
        }
        // Ricochet direction: on past the key, and up off the keyboard.
        let ric = dx / len;
        for _ in 0..26 {
            if !self.room() {
                break;
            }
            let speed = self.rand_range(260.0, 720.0);
            let dirx = ric * self.rand_range(0.2, 1.0) + self.rand_range(-0.5, 0.5);
            let diry = self.rand_range(-1.0, -0.3);
            let l = (dirx * dirx + diry * diry).sqrt().max(0.0001);
            let life = self.rand_range(0.3, 0.7);
            let size = self.rand_range(5.0, 11.0);
            let color = mix(ARC_SPARK, [2.3, 2.3, 2.4], self.rand());
            let x_off = self.rand_range(-key_w * 0.35, key_w * 0.35);
            self.particles.push(Particle {
                x: cx + x_off,
                y,
                vx: dirx / l * speed,
                vy: diry / l * speed,
                life,
                max_life: life,
                size,
                length: 0.0,
                color,
                gravity: GRAVITY,
                style: Style::Spark,
            });
        }
    }

    /// Small dark puff at the key — the anti-celebration.
    fn puff(&mut self, cx: f32, y: f32, color: [f32; 3]) {
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
                color,
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
        self.board_left = board_left;
        self.board_width = board_width;

        self.combo_pop = (self.combo_pop - dt * 4.0).max(0.0);

        for f in &mut self.note_flashes {
            f.life -= dt;
        }
        self.note_flashes.retain(|f| f.life > 0.0);

        for r in &mut self.rising {
            r.age += dt;
        }
        self.rising.retain(|r| r.age < r.life);

        for b in &mut self.bolts {
            b.life -= dt;
        }
        self.bolts.retain(|b| b.life > 0.0);

        self.strike_flash = (self.strike_flash - dt * 3.5).max(0.0);
        self.surge = (self.surge - dt).max(0.0);
        self.surge_phase += dt;

        // Electricity crawling along the hit line for as long as the surge
        // lasts — the keyboard is live, and it should look it.
        if self.surging() {
            self.arc_acc += dt * 95.0;
            while self.arc_acc >= 1.0 {
                self.arc_acc -= 1.0;
                if !self.room() {
                    self.arc_acc = 0.0;
                    break;
                }
                let x = board_left + self.rand() * board_width;
                let life = self.rand_range(0.15, 0.4);
                let y_off = self.rand_range(-6.0, 6.0);
                let vx = self.rand_range(-200.0, 200.0);
                let vy = self.rand_range(-210.0, -40.0);
                let size = self.rand_range(3.0, 8.0);
                let color = mix(ARC_SPARK, [2.2, 2.3, 2.4], self.rand() * 0.8);
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
                    gravity: GRAVITY * 0.35,
                    style: Style::Spark,
                });
            }
        } else {
            self.arc_acc = 0.0;
        }

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

    // --- lightning & surge ---------------------------------------------------

    /// Draw the lightning currently in flight: a wide dim halo under a bright
    /// core along every channel, tapering as it descends and flickering as it
    /// dies. Drawn after everything else, so it is the brightest thing on
    /// screen for the few frames it exists.
    pub fn render_bolts(&self, quads: &mut QuadRenderer) {
        for b in &self.bolts {
            let t = (b.life / b.max_life).clamp(0.0, 1.0);
            let age = b.max_life - b.life;
            // Hard strobe over the top of the fade — lightning does not dim
            // smoothly, it stutters out.
            let flicker = 0.55 + 0.45 * (age * 55.0).sin().abs();
            let alpha = t.powf(0.5) * flicker;

            let segs = (b.path.len() - 1).max(1) as f32;
            for (i, pair) in b.path.windows(2).enumerate() {
                // Fat where it enters the screen, needle-thin at the key.
                let taper = 1.0 - 0.55 * (i as f32 / segs);
                let core = b.width * taper;
                stroke(quads, pair[0], pair[1], core * 3.8, ARC_SPARK, alpha * 0.3);
                stroke(quads, pair[0], pair[1], core, ARC_HOT, alpha);
            }

            for fork in &b.forks {
                for pair in fork.windows(2) {
                    let core = b.width * 0.45;
                    stroke(quads, pair[0], pair[1], core * 3.0, ARC_SPARK, alpha * 0.16);
                    stroke(quads, pair[0], pair[1], core, ARC_HOT, alpha * 0.8);
                }
            }
        }
    }

    /// The surge wash: white-out from a landing bolt, then the keyboard and
    /// the hit line lit electric for as long as the surge holds.
    pub fn render_surge_glow(
        &self,
        quads: &mut QuadRenderer,
        hit_line_y: f32,
        board_left: f32,
        board_width: f32,
        win_w: f32,
        win_h: f32,
    ) {
        // A brief lift over the whole window. Kept light on purpose: enough to
        // feel the room flash, not enough to wash the lane out into milk.
        if self.strike_flash > 0.0 {
            let a = self.strike_flash.powf(1.6) * 0.14;
            quads.push(QuadInstance {
                position: [0.0, 0.0],
                size: [win_w, win_h],
                color: [1.6, 1.9, 2.2, a],
                border_radius: [0.0; 4],
            });
        }

        if !self.surging() {
            return;
        }

        let k = self.surge_intensity();
        let pulse = self.surge_pulse();
        let [r, g, b] = ARC_TINT;

        let breathe = 0.8 + 0.2 * pulse;

        // Light spilling off the hit line, down over the keys and up into the
        // lane. A smooth ramp rather than a few broad bands: the keys stay
        // legible under it, and there is no step in the falloff to see.
        let kb_h = win_h - hit_line_y;
        if kb_h > 0.0 {
            vgradient(
                quads,
                board_left,
                board_width,
                hit_line_y,
                kb_h.min(300.0),
                0.17 * breathe * k,
                [r, g, b],
            );
        }
        vgradient(
            quads,
            board_left,
            board_width,
            hit_line_y,
            -90.0,
            0.095 * breathe * k,
            [r, g, b],
        );

        // The hit line itself: a thin filament with its own tight bloom, so the
        // line reads as the live edge the light is coming off.
        vgradient(
            quads,
            board_left,
            board_width,
            hit_line_y,
            18.0,
            0.10 * breathe * k,
            ARC_SPARK,
        );
        vgradient(
            quads,
            board_left,
            board_width,
            hit_line_y,
            -18.0,
            0.10 * breathe * k,
            ARC_SPARK,
        );
        quads.push(QuadInstance {
            position: [board_left, hit_line_y - 1.5],
            size: [board_width, 3.0],
            color: [
                ARC_HOT[0],
                ARC_HOT[1],
                ARC_HOT[2],
                (0.32 + 0.22 * pulse) * k,
            ],
            border_radius: [1.5; 4],
        });
    }

    /// Electric halo around the falling bars while the surge holds. `bars` are
    /// `(x, width, start_secs, duration_secs)` in the same coordinates
    /// [`Self::render_note_flashes`] uses, and the geometry matches the
    /// waterfall shader so the glow rides exactly with each bar.
    pub fn render_surge_notes<I>(
        &self,
        quads: &mut QuadRenderer,
        bars: I,
        time: f32,
        speed: f32,
        keyboard_y: f32,
    ) where
        I: IntoIterator<Item = (f32, f32, f32, f32)>,
    {
        if !self.surging() {
            return;
        }

        let k = self.surge_intensity();
        let pulse = self.surge_pulse();
        let pad = 4.0 + 3.0 * pulse;
        let [cr, cg, cb] = ARC_TINT;

        for (x, w, start, dur) in bars {
            let bar_h = (dur.max(0.1) - 0.01) * speed.abs();
            let mut y = keyboard_y;
            if speed > 0.0 {
                y -= bar_h;
            }
            y -= (start - time) * speed;

            // Clip to the lane: the keyboard has its own glow, and a bar that
            // has not entered the window yet needs nothing drawn for it.
            let bottom = (y + bar_h).min(keyboard_y);
            let top = y.max(0.0);
            let h = bottom - top;
            if h <= 1.0 {
                continue;
            }

            let (hw, hh) = (w + pad * 2.0, h + pad * 2.0);
            let hr = (hw * 0.25).min(hh * 0.5);
            quads.push(QuadInstance {
                position: [x - pad, top - pad],
                size: [hw, hh],
                color: [cr, cg, cb, (0.22 + 0.16 * pulse) * k],
                border_radius: [hr; 4],
            });

            // A light touch on the bar itself — enough to look lit, not enough
            // to bury the note colour the player reads the lane by.
            let r = (w * 0.2).min(h * 0.5);
            quads.push(QuadInstance {
                position: [x, top],
                size: [w, h],
                color: [cr, cg, cb, (0.05 + 0.06 * pulse) * k],
                border_radius: [r; 4],
            });
        }
    }

    /// Charge pips for the PERFECT chord chain: one per chord needed, lit for
    /// each one banked. Shows the bolt coming so it never lands out of
    /// nowhere. `x`/`y` are the left edge and centre line of the row; returns
    /// the width drawn, so a caller can lay a label out after it.
    pub fn render_bolt_charge(&self, quads: &mut QuadRenderer, x: f32, y: f32) -> f32 {
        const PIP: f32 = 7.0;
        const GAP: f32 = 5.0;

        for i in 0..BOLT_CHORDS {
            let cx = x + PIP * 0.5 + i as f32 * (PIP + GAP);
            if i < self.perfect_chords {
                // Banked: an electric pip with a soft halo around it.
                dot(quads, cx, y, PIP * 2.6, ARC_SPARK, 0.22);
                dot(quads, cx, y, PIP, ARC_HOT, 1.0);
            } else {
                dot(quads, cx, y, PIP, [0.16, 0.18, 0.22], 0.9);
            }
        }

        BOLT_CHORDS as f32 * (PIP + GAP) - GAP
    }

    /// Pulsing halo behind the top-left score readout while surging, so the
    /// number the bonus is inflating is visibly the one being inflated.
    pub fn render_surge_score_glow(&self, quads: &mut QuadRenderer, x: f32, y: f32, w: f32, h: f32) {
        if !self.surging() {
            return;
        }

        let k = self.surge_intensity();
        let pulse = self.surge_pulse();
        let [r, g, b] = ARC_TINT;

        for (grow, alpha) in [(14.0, 0.10), (7.0, 0.16), (2.0, 0.22)] {
            let pad = grow * (0.7 + 0.3 * pulse);
            let (gw, gh) = (w + pad * 2.0, h + pad * 2.0);
            quads.push(QuadInstance {
                position: [x - pad, y - pad],
                size: [gw, gh],
                color: [r, g, b, alpha * (0.6 + 0.4 * pulse) * k],
                border_radius: [(gh * 0.5).min(gw * 0.5); 4],
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

/// Lay a soft vertical glow of `color` across `w` pixels, `peak` alpha at
/// `edge` fading to nothing `depth` pixels away (a negative depth reaches
/// upwards instead).
///
/// The quad renderer has no gradient fill, so the ramp is built from layers —
/// but *nested* ones, all sharing the top edge at `edge` and each reaching a
/// different distance out from it. Their alphas stack into the ramp, and since
/// no two layers share an interior edge there is nothing to double-blend into a
/// visible line. (Tiling the ramp as a column of abutting slices is what draws
/// those lines: every seam is a sliver covered twice.)
fn vgradient(
    quads: &mut QuadRenderer,
    x: f32,
    w: f32,
    edge: f32,
    depth: f32,
    peak: f32,
    color: [f32; 3],
) {
    const LAYERS: usize = 32;

    // Alpha per layer, such that all of them together come to `peak` where they
    // all overlap. Tiny — which is exactly why the step at the far end of each
    // layer is invisible.
    let per = 1.0 - (1.0 - peak).powf(1.0 / LAYERS as f32);

    for i in 1..=LAYERS {
        // How far this layer reaches, as a fraction of `depth`. Chosen so that
        // the number of layers still covering a given distance falls off
        // quadratically — a soft glow that leaves the edge strong and thins out
        // to nothing, rather than a straight linear fade.
        let reach = depth * (1.0 - (1.0 - i as f32 / LAYERS as f32).sqrt());
        if reach.abs() < 0.5 {
            continue;
        }

        quads.push(QuadInstance {
            position: [x, edge.min(edge + reach)],
            size: [w, reach.abs()],
            color: [color[0], color[1], color[2], per],
            border_radius: [0.0; 4],
        });
    }
}

/// Lay a chain of overlapping dots of diameter `d` from `a` to `b`. The quad
/// renderer cannot rotate a bar to lie along a diagonal, so a diagonal stroke
/// of light is drawn as closely-spaced circles instead.
fn stroke(quads: &mut QuadRenderer, a: [f32; 2], b: [f32; 2], d: f32, color: [f32; 3], alpha: f32) {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt();
    // Spacing well under the radius, so the chain reads as one solid stroke.
    let steps = (len / (d * 0.3).max(1.0)).ceil().max(1.0);
    for i in 0..=steps as usize {
        let t = i as f32 / steps;
        dot(quads, a[0] + dx * t, a[1] + dy * t, d, color, alpha);
    }
}

/// 1234567 -> "1,234,567" for the score displays.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

#[cfg(test)]
mod tests {
    use super::{BOLT_CHORDS, EffectsSystem, Results};
    use std::time::{Duration, Instant};

    /// Strike a correct, well-timed note that the song asked for at `chord`.
    fn hit(fx: &mut EffectsSystem, note: u8, chord: Instant) {
        fx.good_hit(note, 0.0, 100.0, 20.0, 0.01, false, Some(chord));
    }

    /// The same, `delta` seconds off the beat — 0.2 is a GOOD, not a PERFECT.
    fn hit_off(fx: &mut EffectsSystem, note: u8, chord: Instant, delta: f32) {
        fx.good_hit(note, 0.0, 100.0, 20.0, delta, false, Some(chord));
    }

    /// Points a chord struck dead on time is actually credited with.
    fn pay_for_perfect(fx: &mut EffectsSystem, chord: Instant) -> u64 {
        let before = fx.score();
        hit(fx, 60, chord);
        fx.score() - before
    }

    /// `n` separate chords, each one note struck dead on time.
    fn perfect_chords(fx: &mut EffectsSystem, start: Instant, n: u64) {
        for i in 0..n {
            hit(fx, 60, start + Duration::from_millis(100 * i));
        }
    }

    /// Three keys down for one chord is one musical event, so one combo step —
    /// however ragged the hands were about it.
    #[test]
    fn a_chord_advances_the_combo_once() {
        let mut fx = EffectsSystem::new();
        let chord = Instant::now();

        hit(&mut fx, 60, chord);
        hit(&mut fx, 64, chord);
        hit(&mut fx, 67, chord);

        assert_eq!(fx.combo(), 1);
        assert_eq!(fx.best_combo(), 1);
    }

    /// The song voicing a chord a few ms wide, or the player rolling it, must
    /// not turn one chord into several combo steps.
    #[test]
    fn a_rolled_chord_is_still_one_chord() {
        let mut fx = EffectsSystem::new();
        let chord = Instant::now();

        hit(&mut fx, 60, chord);
        hit(&mut fx, 64, chord + Duration::from_millis(12));
        hit(&mut fx, 67, chord + Duration::from_millis(25));

        assert_eq!(fx.combo(), 1);
    }

    /// Separate notes stay separate — a fast run must still build a streak.
    #[test]
    fn consecutive_notes_each_advance_the_combo() {
        let mut fx = EffectsSystem::new();
        let start = Instant::now();

        for (i, note) in [60u8, 62, 64, 65].into_iter().enumerate() {
            hit(&mut fx, note, start + Duration::from_millis(100 * i as u64));
        }

        assert_eq!(fx.combo(), 4);
    }

    /// 32nd notes at 120bpm are ~62ms apart and are not a chord.
    #[test]
    fn fast_runs_are_not_mistaken_for_chords() {
        let mut fx = EffectsSystem::new();
        let start = Instant::now();

        hit(&mut fx, 60, start);
        hit(&mut fx, 62, start + Duration::from_millis(62));

        assert_eq!(fx.combo(), 2);
    }

    /// Chords count once, but each key is still judged on its own timing —
    /// the streak changes, the note tallies do not.
    #[test]
    fn chord_notes_are_still_scored_individually() {
        let mut fx = EffectsSystem::new();
        let chord = Instant::now();

        hit(&mut fx, 60, chord);
        hit(&mut fx, 64, chord);
        hit(&mut fx, 67, chord);

        let results = fx.results();
        assert_eq!(fx.combo(), 1);
        assert_eq!(results.perfect, 3);
        assert_eq!(results.total_hit(), 3);
    }

    fn run(perfect: u32, good: u32, ok: u32, wrong: u32) -> &'static str {
        Results {
            perfect,
            good,
            ok,
            wrong,
            best_combo: 0,
            missed: 0,
            score: 0,
        }
        .grade()
        .0
    }

    fn run_with_misses(perfect: u32, missed: u32) -> &'static str {
        Results {
            perfect,
            good: 0,
            ok: 0,
            wrong: 0,
            missed,
            best_combo: 0,
            score: 0,
        }
        .grade()
        .0
    }

    /// The grade ladder is only meaningful if these stay pinned: an A has to
    /// mean "played in time", not "eventually hit the right keys" — but
    /// playing a song *well* has to actually land in the A/B range.
    #[test]
    fn grades_reward_timing_not_just_correctness() {
        // Flawless timing is the only way to the very top of the ladder.
        assert_eq!(run(100, 0, 0, 0), "A++");
        assert_eq!(run(85, 12, 2, 1), "A+");

        // Right notes, consistently a shade behind: a clean performance.
        assert_eq!(run(0, 100, 0, 0), "A-");

        // Loose but competent.
        assert_eq!(run(60, 25, 10, 5), "A-");

        // A scrappy run — sloppy timing plus a lot of wrong notes.
        assert_eq!(run(27, 28, 25, 20), "C");

        // The song stalled and waited for every single note. That is not a
        // performance, and it should not flatter one.
        assert_eq!(run(0, 0, 100, 0), "D+");
    }

    /// Wrong notes have to cost more than the slot they take up, or spraying
    /// extra notes is nearly free as long as the right ones land too.
    #[test]
    fn wrong_notes_are_penalised_beyond_dilution() {
        // Same 100 perfectly-timed notes each time; only the spray changes.
        assert_eq!(run(100, 0, 0, 0), "A++");
        assert_eq!(run(100, 0, 0, 20), "B+");
        assert_eq!(run(100, 0, 0, 50), "C+");
    }

    #[test]
    fn empty_run_does_not_divide_by_zero() {
        assert_eq!(run(0, 0, 0, 0), "F");
    }

    /// Misses are zero-credit attempts: they dilute the grade like wrong
    /// notes but at half weight and without the extra penalty, so sitting
    /// out part of a jam costs less than fumbling it. Playing nothing at all
    /// in jam mode is still an F — though the results screen never shows for
    /// it, because misses alone are not activity.
    #[test]
    fn misses_dilute_the_grade_without_extra_penalty() {
        assert_eq!(run_with_misses(100, 0), "A++");
        assert_eq!(run_with_misses(90, 10), "A+");
        // Jamming along with half the notes is a solid showing, not a fail.
        assert_eq!(run_with_misses(50, 50), "B");
        assert_eq!(run_with_misses(0, 100), "F");

        let mut fx = EffectsSystem::new();
        fx.miss();
        fx.miss();
        assert!(!fx.has_activity());
        assert_eq!(fx.results().missed, 2);
    }

    /// Hits earn timing points, multiplied once the combo passes each tier of
    /// 10 — so the 10th consecutive hit is the first one worth double. Graded
    /// on GOOD hits (60 points) to keep the lightning out of it: notes a shade
    /// behind never charge the chain, so this is the combo tier alone.
    #[test]
    fn score_applies_the_combo_multiplier() {
        let mut fx = EffectsSystem::new();
        let start = Instant::now();

        for i in 0..10u64 {
            hit_off(&mut fx, 60, start + Duration::from_millis(100 * i), 0.2);
        }

        assert!(!fx.surging());
        assert_eq!(fx.score(), 9 * 60 + 60 * 2);
    }

    /// Every note of a chord scores — the combo counts a chord once, but a
    /// three-key chord is still worth three notes of points.
    #[test]
    fn chord_notes_each_score() {
        let mut fx = EffectsSystem::new();
        let chord = Instant::now();

        hit(&mut fx, 60, chord);
        hit(&mut fx, 64, chord);
        hit(&mut fx, 67, chord);

        assert_eq!(fx.combo(), 1);
        assert_eq!(fx.score(), 300);
    }

    /// A full chain of chords nailed dead-on calls the lightning down — on the
    /// chord that completes it, and not one chord sooner.
    #[test]
    fn a_chain_of_perfect_chords_calls_the_lightning() {
        let mut fx = EffectsSystem::new();
        let start = Instant::now();

        let short = BOLT_CHORDS as u64 - 1;
        perfect_chords(&mut fx, start, short);
        assert!(!fx.surging());
        assert_eq!(fx.perfect_chords(), BOLT_CHORDS - 1);

        hit(&mut fx, 60, start + Duration::from_millis(100 * short));
        assert!(fx.surging());
        // The chain starts over, so a long clean run keeps re-striking.
        assert_eq!(fx.perfect_chords(), 0);
    }

    /// The surge cannot be extended by playing well through it: the chain is
    /// frozen while the lights are up, so the reward has to run out and then be
    /// earned again from nothing.
    #[test]
    fn a_surge_cannot_be_renewed_before_it_ends() {
        let mut fx = EffectsSystem::new();
        let start = Instant::now();

        perfect_chords(&mut fx, start, BOLT_CHORDS as u64);
        assert!(fx.surging());
        let struck_at = fx.surge_secs_left();

        // A whole clean chain played inside the surge banks nothing and, above
        // all, does not top the timer back up.
        fx.update(2.0, 100.0, 0.0, 500.0);
        perfect_chords(
            &mut fx,
            start + Duration::from_millis(100 * BOLT_CHORDS as u64),
            BOLT_CHORDS as u64,
        );
        assert_eq!(fx.perfect_chords(), 0);
        assert!(fx.surge_secs_left() < struck_at);

        // Once it expires, the same playing earns a fresh strike.
        fx.update(super::SURGE_SECS, 100.0, 0.0, 500.0);
        assert!(!fx.surging());

        let later = start + Duration::from_secs(30);
        perfect_chords(&mut fx, later, BOLT_CHORDS as u64 - 1);
        assert!(!fx.surging());
        hit(
            &mut fx,
            60,
            later + Duration::from_millis(100 * BOLT_CHORDS as u64),
        );
        assert!(fx.surging());
    }

    /// The chain is about *timing*, not correctness: right notes played a
    /// shade behind never charge it, however long the streak.
    #[test]
    fn only_perfect_timing_charges_the_chain() {
        let mut fx = EffectsSystem::new();
        let start = Instant::now();

        for i in 0..8u64 {
            hit_off(&mut fx, 60, start + Duration::from_millis(100 * i), 0.2);
        }

        assert_eq!(fx.combo(), 8);
        assert_eq!(fx.perfect_chords(), 0);
        assert!(!fx.surging());
    }

    /// A chord is only PERFECT if every key in it was: one late hand spoils
    /// the chord and empties the chain it was building.
    #[test]
    fn a_late_key_spoils_the_chord_it_belongs_to() {
        let mut fx = EffectsSystem::new();
        let start = Instant::now();

        // Two keys of one chord, the second of them behind the beat. The first
        // opened the chord's account; the second empties it again.
        hit(&mut fx, 60, start);
        assert_eq!(fx.perfect_chords(), 1);
        hit_off(&mut fx, 64, start, 0.2);
        assert_eq!(fx.perfect_chords(), 0);

        // So a full chain is owed from scratch: one short of it is still dark.
        let next = start + Duration::from_millis(100);
        perfect_chords(&mut fx, next, BOLT_CHORDS as u64 - 1);
        assert!(!fx.surging());

        hit(&mut fx, 60, next + Duration::from_millis(100 * BOLT_CHORDS as u64));
        assert!(fx.surging());
    }

    /// Every kind of mistake empties the chain.
    #[test]
    fn mistakes_empty_the_chain() {
        let start = Instant::now();

        let mut fx = EffectsSystem::new();
        perfect_chords(&mut fx, start, BOLT_CHORDS as u64 - 1);
        fx.wrong_hit(0.0, 100.0);
        assert_eq!(fx.perfect_chords(), 0);

        let mut fx = EffectsSystem::new();
        perfect_chords(&mut fx, start, BOLT_CHORDS as u64 - 1);
        fx.miss();
        assert_eq!(fx.perfect_chords(), 0);

        // A note the song had to stall and wait for, too.
        let mut fx = EffectsSystem::new();
        perfect_chords(&mut fx, start, BOLT_CHORDS as u64 - 1);
        fx.good_hit(60, 0.0, 100.0, 20.0, 0.9, true, Some(start));
        assert_eq!(fx.perfect_chords(), 0);
    }

    /// While the surge holds, every note is worth half again as much — the
    /// note that summoned the bolt included — and it expires on its own.
    #[test]
    fn the_surge_pays_half_again_per_note() {
        let mut fx = EffectsSystem::new();
        let start = Instant::now();

        // One chord short of the chain, so the combo tier is still x1 and each
        // note is worth its face value of 100.
        let mut at = BOLT_CHORDS as u64 - 1;
        perfect_chords(&mut fx, start, at);
        assert_eq!(fx.score(), at * 100);
        let mut next = || {
            at += 1;
            start + Duration::from_millis(100 * at)
        };

        // The chord that summons the bolt is itself paid at the surge rate.
        assert_eq!(pay_for_perfect(&mut fx, next()), 150);
        assert!(fx.surging());
        assert_eq!(pay_for_perfect(&mut fx, next()), 150);

        // Run the clock past the window: the lights go out.
        fx.update(super::SURGE_SECS + 0.1, 100.0, 0.0, 500.0);
        assert!(!fx.surging());

        // One note off the beat, to empty the chain the surged notes were
        // quietly rebuilding — otherwise the very next PERFECT could complete
        // it and re-strike, which is not what is under test here.
        hit_off(&mut fx, 60, next(), 0.2);

        // And notes are back to face value.
        assert_eq!(pay_for_perfect(&mut fx, next()), 100);
    }

    #[test]
    fn thousands_formats_groups() {
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(999), "999");
        assert_eq!(super::thousands(1000), "1,000");
        assert_eq!(super::thousands(1234567), "1,234,567");
    }
}
