//! A discreet neon-green sheet-music strip across the top of the playing
//! screen.
//!
//! The song scrolls right to left and the note under the playhead — dead
//! centre — is the one due now, so what the waterfall says with falling bars
//! this says in staff notation. It occupies only the middle of the screen, so
//! the streak counter on the left and the mode buttons on the right keep
//! their space, and it sits on a black wash that the waterfall still shows
//! faintly through. Everything about it — the wash, the staff rules, the
//! notes — fades out towards both ends, so it reads as part of the scene
//! rather than a panel bolted onto it.
//!
//! Layout is proportional to time rather than engraved into bars, which is
//! what lets it scroll continuously and sidesteps most of what makes MIDI ->
//! notation hard. The part that is left — spelling pitches onto lines and
//! spaces, and choosing noteheads — lives in [`engrave`].
//!
//! Symbols are drawn with **Bravura**, the reference font for
//! [SMuFL](https://w3c.github.io/smufl/), the standard music-font layout that
//! Dorico, MuseScore and Verovio all target. Using it means the clefs,
//! noteheads, accidentals and flags are real engraved shapes rather than
//! approximations, and the geometry below can lean on SMuFL's two guarantees:
//! one em is four staff spaces, and a glyph's origin sits on its staff
//! position. Staff lines, stems, ledger lines and bar lines are plain quads.

mod engrave;

pub use engrave::{Accidental, Head, Score, SheetNote, SheetStem, Staff, Staves};

use std::time::Duration;

use midi_file::MidiNote;

use crate::{
    render::{QuadInstance, QuadRenderer, TextRenderer},
    utils::Rect,
};

/// SMuFL codepoints, in the order the `G_*` indices below use them.
const SMUFL: [char; 12] = [
    '\u{E0A2}', // noteheadWhole
    '\u{E0A3}', // noteheadHalf
    '\u{E0A4}', // noteheadBlack
    '\u{E260}', // accidentalFlat
    '\u{E261}', // accidentalNatural
    '\u{E262}', // accidentalSharp
    '\u{E050}', // gClef
    '\u{E062}', // fClef
    '\u{E240}', // flag8thUp
    '\u{E241}', // flag8thDown
    '\u{E242}', // flag16thUp
    '\u{E243}', // flag16thDown
];

const G_WHOLE: usize = 0;
const G_HALF: usize = 1;
const G_BLACK: usize = 2;
const G_FLAT: usize = 3;
const G_NATURAL: usize = 4;
const G_SHARP: usize = 5;
const G_CLEF_G: usize = 6;
const G_CLEF_F: usize = 7;
const G_FLAG8_UP: usize = 8;
const G_FLAG8_DOWN: usize = 9;
const G_FLAG16_UP: usize = 10;
const G_FLAG16_DOWN: usize = 11;

/// Gap between two staff lines, in logical pixels. Every other measurement
/// here is a multiple of it, as in real engraving.
const STAFF_SPACE: f32 = 9.0;
/// Half a staff space: one diatonic step, line to adjacent space.
const STEP: f32 = STAFF_SPACE / 2.0;
/// Breathing room at the very top and bottom of the strip.
const MARGIN: f32 = 6.0;
/// Ceiling on the ledger-line room reserved beyond the staff, in staff
/// positions, so an outlier note can't stretch the strip down the screen.
const MAX_PAD_STEPS: i32 = 12;

/// Share of the window width the strip spans, centred. The margins either
/// side belong to the HUD: streak and score on the left, mode buttons on the
/// right.
const BAND_FRACTION: f32 = 0.5;
/// Seconds of music visible across the strip. Half is still to come, half has
/// just been played.
const SECONDS_ACROSS: f32 = 6.0;
/// Width at the left of the strip for the clef and key signature, which stay
/// put while the music scrolls towards them.
const GUTTER: f32 = 58.0;
/// Distance over which the strip fades in and out at its two ends. Nothing
/// here has a border to hide a hard clip behind, so the music — and the wash
/// under it — has to arrive and leave gently.
const FADE: f32 = 80.0;
/// Opacity of the black wash behind the staff. Enough for the notation to
/// read cleanly, little enough that the waterfall still shows through it.
const BACKDROP_ALPHA: f32 = 0.74;

const NEON: [f32; 3] = [0.25, 1.0, 0.55];
const NEON_BRIGHT: [f32; 3] = [0.72, 1.0, 0.85];

fn rgba(rgb: [f32; 3], a: f32) -> [f32; 4] {
    [rgb[0], rgb[1], rgb[2], a]
}

fn text_color(c: [f32; 4]) -> glyphon::Color {
    glyphon::Color::rgba(
        (c[0] * 255.0) as u8,
        (c[1] * 255.0) as u8,
        (c[2] * 255.0) as u8,
        (c[3] * 255.0) as u8,
    )
}

/// One Bravura glyph, shaped once at startup and reused every frame.
struct Glyph {
    buffer: glyphon::Buffer,
    /// Advance width, used to centre noteheads on their beat.
    width: f32,
    /// Distance from the top of the buffer down to the baseline. SMuFL puts a
    /// glyph's origin on its staff position, so this is what turns a staff
    /// position into a `top` for glyphon.
    baseline: f32,
}

/// A glyph queued for this frame: which one, where, and in what colour.
struct Placed {
    id: usize,
    left: f32,
    top: f32,
    color: glyphon::Color,
}

/// Horizontal extent of the strip for the current window size.
#[derive(Clone, Copy)]
struct Band {
    left: f32,
    right: f32,
    /// The playhead, and the window centre.
    centre: f32,
    /// Pixels per second of music.
    pps: f32,
    /// Where scrolling music starts, just right of the clef.
    notes_left: f32,
}

impl Band {
    fn new(window_width: f32) -> Self {
        let width = window_width * BAND_FRACTION;
        let left = (window_width - width) / 2.0;
        Self {
            left,
            right: left + width,
            centre: window_width / 2.0,
            pps: width / SECONDS_ACROSS,
            notes_left: left + GUTTER,
        }
    }

    fn x_of(&self, t: f32, now: f32) -> f32 {
        self.centre + (t - now) * self.pps
    }

    /// How visible something at `x` is: notes dissolve into the clef on the
    /// left and appear out of nothing on the right.
    fn fade(&self, x: f32) -> f32 {
        let in_ = (x - self.notes_left) / FADE;
        let out = (self.right - x) / FADE;
        in_.min(out).clamp(0.0, 1.0)
    }
}

pub struct SheetMusic {
    score: Score,
    glyphs: Vec<Glyph>,
    text_renderer: TextRenderer,
    quad_renderer: QuadRenderer,
    placed: Vec<Placed>,

    /// Vertical position of middle C. Both staves hang off it: measuring
    /// every pitch from middle C gives the grand staff its correct spacing
    /// for free, with middle C's ledger line exactly halfway between the two.
    middle_c_y: f32,
    height: f32,
    /// How far the whole strip is pushed down this frame, so the expanding
    /// top bar slides it out of the way instead of covering it.
    y_offset: f32,
}

impl SheetMusic {
    /// `quarters_per_bar` comes from the file's time signature; it sets both
    /// where the bar lines fall and how long a beat is.
    pub fn new(
        notes: &[MidiNote],
        measures: &[Duration],
        quarters_per_bar: f32,
        text_renderer: TextRenderer,
        quad_renderer: QuadRenderer,
    ) -> Self {
        let score = engrave::engrave(notes, measures, quarters_per_bar);

        // Size the strip to what the song actually reaches, so a right-hand
        // melody gets a shallow staff and only a piece that really does climb
        // onto ledger lines pays for the room. Capped so one stray piccolo
        // note can't push the staff down the screen — that note gets clipped
        // instead.
        let (top_line, bottom_line) = score.staves.extent();
        let (min_top, min_bottom) = score.staves.min_padding_steps();
        let pad_top = (score.top_step - top_line + 1).clamp(min_top, MAX_PAD_STEPS) as f32;
        let pad_bottom =
            (bottom_line - score.bottom_step + 1).clamp(min_bottom, MAX_PAD_STEPS) as f32;

        let span = (top_line - bottom_line) as f32 * STEP;
        let height = span + (pad_top + pad_bottom) * STEP + 2.0 * MARGIN;
        let middle_c_y = MARGIN + pad_top * STEP + top_line as f32 * STEP;

        Self {
            score,
            glyphs: load_glyphs(),
            text_renderer,
            quad_renderer,
            placed: Vec::new(),
            middle_c_y,
            height,
            y_offset: 0.0,
        }
    }

    /// Height of the strip in logical pixels.
    pub fn height(&self) -> f32 {
        self.height
    }

    pub fn is_empty(&self) -> bool {
        self.score.notes.is_empty()
    }

    fn y_of(&self, step: f32) -> f32 {
        self.y_offset + self.middle_c_y - step * STEP
    }

    fn staves(&self) -> &'static [Staff] {
        match self.score.staves {
            Staves::Treble => &[Staff::Treble],
            Staves::Bass => &[Staff::Bass],
            Staves::Grand => &[Staff::Treble, Staff::Bass],
        }
    }

    fn push_glyph(&mut self, id: usize, left: f32, step: f32, color: [f32; 4]) {
        if color[3] <= 0.01 {
            return;
        }
        let baseline = self.glyphs[id].baseline;
        let top = self.y_of(step) - baseline;
        self.placed.push(Placed {
            id,
            left,
            top,
            color: text_color(color),
        });
    }

    #[profiling::function]
    pub fn update(
        &mut self,
        physical_size: dpi::PhysicalSize<u32>,
        scale: f32,
        time: f32,
        size: dpi::LogicalSize<f32>,
        y_offset: f32,
    ) {
        self.placed.clear();
        self.y_offset = y_offset;

        let band = Band::new(size.width);
        let mut quads: Vec<QuadInstance> = Vec::new();

        self.draw_backdrop(&mut quads, band);
        self.draw_staff_lines(&mut quads, band);
        self.draw_barlines(&mut quads, band, time);
        self.draw_notes(&mut quads, band, time);
        self.draw_gutter(&mut quads, band);
        self.draw_playhead(&mut quads, band);

        // Clip to the strip so nothing reaches the HUD to either side or the
        // waterfall below. Oversized scissor rects are a wgpu validation
        // error, so clamp to the surface.
        let x = ((band.left * scale) as u32).min(physical_size.width);
        let y = ((self.y_offset.max(0.0) * scale) as u32).min(physical_size.height);
        let w = ((band.right - band.left) * scale) as u32;
        let h = ((self.height * scale) as u32).min(physical_size.height - y);
        let clip = Rect::new(
            (x, y).into(),
            (w.min(physical_size.width - x), h).into(),
        );

        self.quad_renderer.clear();
        self.quad_renderer.layer().extend(quads);
        self.quad_renderer.set_scissor_rect(clip);
        self.quad_renderer.prepare();

        self.text_renderer.set_scissor_rect(clip);

        let bounds = glyphon::TextBounds {
            left: band.left as i32,
            top: self.y_offset as i32,
            right: band.right.ceil() as i32,
            bottom: (self.y_offset + self.height).ceil() as i32,
        };

        let glyphs = &self.glyphs;
        let areas = self.placed.iter().map(|p| glyphon::TextArea {
            buffer: &glyphs[p.id].buffer,
            left: p.left,
            top: p.top,
            scale: 1.0,
            bounds,
            default_color: p.color,
            custom_glyphs: &[],
        });

        self.text_renderer
            .update_from_iter(physical_size, scale, areas);
    }

    /// A black wash behind the staff so the notation reads against the
    /// waterfall. Deliberately short of opaque: the falling bars should still
    /// be faintly visible through it, and it tapers off at both ends so the
    /// strip never looks like a pasted-on box.
    fn draw_backdrop(&self, quads: &mut Vec<QuadInstance>, band: Band) {
        self.for_each_segment(band, |x, w, taper| {
            quads.push(QuadInstance {
                position: [x, self.y_offset],
                size: [w, self.height],
                color: [0.0, 0.0, 0.0, BACKDROP_ALPHA * taper],
                border_radius: [0.0; 4],
            });
        });
    }

    /// Walk the strip left to right in slices, handing each one a taper that
    /// falls to nothing at the two ends. Fading a quad across its own width
    /// isn't something the shared quad shader can do, so anything that spans
    /// the strip is built from slices instead.
    fn for_each_segment(&self, band: Band, mut f: impl FnMut(f32, f32, f32)) {
        const SEGMENTS: usize = 28;
        let seg_w = (band.right - band.left) / SEGMENTS as f32;

        for i in 0..SEGMENTS {
            let x = band.left + i as f32 * seg_w;
            let taper = ((x - band.left) / FADE)
                .min((band.right - (x + seg_w)) / FADE)
                .clamp(0.0, 1.0);
            if taper > 0.01 {
                // Overlap by a pixel so no seams show between slices.
                f(x, seg_w + 1.0, taper);
            }
        }
    }

    /// The five rules per staff — hairlines, dim, and tapering away at each
    /// end so the strip has no hard edges.
    fn draw_staff_lines(&self, quads: &mut Vec<QuadInstance>, band: Band) {
        for staff in self.staves() {
            for step in staff.line_steps() {
                // Tapered on distance from the strip's ends, not on
                // `Band::fade`, so the rules run on behind the clef.
                let y = self.y_of(step as f32) - 0.5;
                self.for_each_segment(band, |x, w, taper| {
                    quads.push(QuadInstance {
                        position: [x, y],
                        size: [w, 1.0],
                        color: rgba(NEON, 0.30 * taper),
                        border_radius: [0.0; 4],
                    });
                });
            }
        }
    }

    fn draw_barlines(&self, quads: &mut Vec<QuadInstance>, band: Band, time: f32) {
        let (top, bottom) = self.score.staves.extent();
        let y_top = self.y_of(top as f32);
        let y_bottom = self.y_of(bottom as f32);

        for t in &self.score.barlines {
            let x = band.x_of(*t, time);
            if x < band.notes_left {
                continue;
            }
            if x > band.right {
                break;
            }
            let fade = band.fade(x);
            quads.push(QuadInstance {
                position: [x, y_top],
                size: [1.0, y_bottom - y_top],
                color: rgba(NEON, 0.18 * fade),
                border_radius: [0.0; 4],
            });
        }
    }

    /// The playhead: whatever sits on this line is due now.
    fn draw_playhead(&self, quads: &mut Vec<QuadInstance>, band: Band) {
        quads.push(QuadInstance {
            position: [band.centre - 3.0, self.y_offset],
            size: [6.0, self.height],
            color: rgba(NEON_BRIGHT, 0.07),
            border_radius: [0.0; 4],
        });
        quads.push(QuadInstance {
            position: [band.centre - 0.5, self.y_offset],
            size: [1.0, self.height],
            color: rgba(NEON_BRIGHT, 0.5),
            border_radius: [0.0; 4],
        });
    }

    /// Clef and key signature, pinned to the left so they stay readable while
    /// the music scrolls in towards them.
    fn draw_gutter(&mut self, _quads: &mut Vec<QuadInstance>, band: Band) {
        let staves = self.staves();
        for staff in staves {
            // A G clef curls around the G above middle C; an F clef's two
            // dots straddle the F below it.
            let (id, step) = match staff {
                Staff::Treble => (G_CLEF_G, 4.0),
                Staff::Bass => (G_CLEF_F, -4.0),
            };
            self.push_glyph(id, band.left + 4.0, step, rgba(NEON, 0.75));

            let mut x = band.left + 4.0 + self.glyphs[id].width + 4.0;
            let signature = self.score.key_signature(*staff);
            for (sig_step, acc) in signature {
                let gid = accidental_glyph(acc);
                let w = self.glyphs[gid].width;
                if x + w > band.left + GUTTER - 2.0 {
                    break;
                }
                self.push_glyph(gid, x, sig_step as f32, rgba(NEON, 0.7));
                x += w * 0.92;
            }
        }
    }

    fn draw_notes(&mut self, quads: &mut Vec<QuadInstance>, band: Band, time: f32) {
        let half_window = SECONDS_ACROSS / 2.0;
        let from = time - half_window - 1.0;
        let to = time + half_window + 1.0;

        let head_w = self.glyphs[G_BLACK].width;

        // Both lists are start-sorted, so what is on screen is a contiguous
        // slice of each.
        let first = self.score.notes.partition_point(|n| n.start < from);
        let notes: Vec<SheetNote> = self.score.notes[first..]
            .iter()
            .take_while(|n| n.start <= to)
            .copied()
            .collect();

        for n in &notes {
            if !self.score.staves.contains(n.staff) {
                continue;
            }

            let x = band.x_of(n.start, time);
            let fade = band.fade(x);
            if fade <= 0.01 {
                continue;
            }

            let sounding = n.start <= time && time <= n.end;
            let past = n.end < time;

            let alpha = fade
                * if sounding {
                    1.0
                } else if past {
                    0.35
                } else {
                    0.9
                };
            let color = if sounding { NEON_BRIGHT } else { NEON };

            let step = n.step as f32;
            let y = self.y_of(step);
            let shift = if n.head_shift { head_w } else { 0.0 };
            let head_left = x - head_w / 2.0 + shift;

            // How long the key is actually held, as a soft slab behind the
            // notehead. The notated value is a rounding of this; the slab is
            // the truth, and it gives the eye the same rolling cue the
            // waterfall does. It has to be thick and faint rather than thin
            // and bright, or at note height it reads as another staff line.
            let x_end = band.x_of(n.end, time).min(band.right);
            let bar_w = x_end - x;
            if bar_w > head_w * 0.9 {
                let h = STAFF_SPACE * 0.7;
                quads.push(QuadInstance {
                    position: [x, y - h / 2.0],
                    size: [bar_w, h],
                    color: rgba(color, fade * if sounding { 0.26 } else { 0.09 }),
                    border_radius: [h / 2.0; 4],
                });
            }

            self.draw_ledgers(quads, x, n, head_w, rgba(color, alpha * 0.8));

            if let Some(acc) = n.accidental {
                let gid = accidental_glyph(acc);
                let w = self.glyphs[gid].width;
                self.push_glyph(gid, head_left - w - STEP * 0.4, step, rgba(color, alpha));
            }

            let gid = match n.head {
                Head::Whole => G_WHOLE,
                Head::Half => G_HALF,
                Head::Black => G_BLACK,
            };
            self.push_glyph(gid, head_left, step, rgba(color, alpha));

            if n.dotted {
                // Dots live in a space; a note on a line lifts its dot to the
                // space above.
                let dot_step = if n.step % 2 == 0 { step + 1.0 } else { step };
                let r = STEP * 0.32;
                quads.push(QuadInstance {
                    position: [head_left + head_w + STEP * 0.45, self.y_of(dot_step) - r],
                    size: [r * 2.0, r * 2.0],
                    color: rgba(color, alpha),
                    border_radius: [r; 4],
                });
            }

            if sounding {
                // A halo, so the note under the playhead reads at a glance.
                let r = STAFF_SPACE * 0.9;
                quads.push(QuadInstance {
                    position: [head_left + head_w / 2.0 - r, y - r],
                    size: [r * 2.0, r * 2.0],
                    color: rgba(NEON_BRIGHT, 0.2 * fade),
                    border_radius: [r; 4],
                });
            }
        }

        // Stems and flags last, so they sit over the heads.
        let first = self.score.stems.partition_point(|s| s.start < from);
        let stems: Vec<SheetStem> = self.score.stems[first..]
            .iter()
            .take_while(|s| s.start <= to)
            .copied()
            .collect();

        for s in &stems {
            if !self.score.staves.contains(s.staff) {
                continue;
            }

            let x = band.x_of(s.start, time);
            let fade = band.fade(x);
            if fade <= 0.01 {
                continue;
            }

            let past = s.start < time - 0.05;
            let color = rgba(NEON, fade * if past { 0.35 } else { 0.85 });

            let shift = if s.base_shifted { head_w } else { 0.0 };
            // Stems rise from the right-hand edge of the notehead and fall
            // from the left, tucked just inside so they don't poke through.
            let stem_x = if s.down {
                x - head_w / 2.0 + shift + 0.6
            } else {
                x + head_w / 2.0 + shift - 1.7
            };

            let y_base = self.y_of(s.base_step as f32);
            let y_tip = self.y_of(s.tip_step);
            let (y0, h) = if s.down {
                (y_base, y_tip - y_base)
            } else {
                (y_tip, y_base - y_tip)
            };

            quads.push(QuadInstance {
                position: [stem_x, y0],
                size: [1.3, h.abs()],
                color,
                border_radius: [0.0; 4],
            });

            if s.flags > 0 {
                let gid = match (s.flags, s.down) {
                    (1, false) => G_FLAG8_UP,
                    (1, true) => G_FLAG8_DOWN,
                    (_, false) => G_FLAG16_UP,
                    (_, true) => G_FLAG16_DOWN,
                };
                // A flag's origin is where it meets the end of the stem.
                self.push_glyph(gid, stem_x, s.tip_step, color);
            }
        }
    }

    /// Ledger lines bridging the staff and a note that has strayed off it.
    fn draw_ledgers(
        &self,
        quads: &mut Vec<QuadInstance>,
        x: f32,
        n: &SheetNote,
        head_w: f32,
        color: [f32; 4],
    ) {
        let lines = n.staff.line_steps();
        let (low, high) = (lines[0], lines[4]);
        let w = head_w * 1.6;

        let mut push = |step: i32| {
            quads.push(QuadInstance {
                position: [x - w / 2.0, self.y_of(step as f32) - 0.5],
                size: [w, 1.0],
                color,
                border_radius: [0.0; 4],
            });
        };

        // Ledger lines only ever land on even staff positions.
        if n.step < low {
            let mut s = low - 2;
            while s >= n.step {
                push(s);
                s -= 2;
            }
        } else if n.step > high {
            let mut s = high + 2;
            while s <= n.step {
                push(s);
                s += 2;
            }
        }
    }

    pub fn render<'rpass>(&'rpass mut self, render_pass: &mut wgpu_jumpstart::RenderPass<'rpass>) {
        self.quad_renderer.render(render_pass);
        self.text_renderer.render(render_pass);
    }
}

fn accidental_glyph(acc: Accidental) -> usize {
    match acc {
        Accidental::Sharp => G_SHARP,
        Accidental::Flat => G_FLAT,
        Accidental::Natural => G_NATURAL,
    }
}

fn load_glyphs() -> Vec<Glyph> {
    let font_system = crate::font_system::font_system();
    let font_system = &mut font_system.borrow_mut();

    // SMuFL glyphs are drawn on an em of four staff spaces.
    let font_size = STAFF_SPACE * 4.0;

    SMUFL
        .iter()
        .map(|ch| {
            let mut buffer = glyphon::Buffer::new(
                font_system,
                // Generous line height: a G clef spans seven staff spaces, so
                // a line box the height of the em would lay it out cramped.
                glyphon::Metrics::new(font_size, font_size * 3.0),
            );
            buffer.set_size(Some(f32::MAX), Some(f32::MAX));
            buffer.set_wrap(glyphon::Wrap::None);
            buffer.set_text(
                ch.encode_utf8(&mut [0u8; 4]),
                &glyphon::Attrs::new().family(glyphon::Family::Name("Bravura")),
                glyphon::Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(font_system, false);

            let (width, baseline) = buffer
                .layout_runs()
                .next()
                .map(|run| (run.line_w, run.line_y))
                .unwrap_or((font_size * 0.3, font_size));

            Glyph {
                buffer,
                width,
                baseline,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bravura has to actually be in the font database, and cosmic-text has
    /// to shape its private-use codepoints into real glyphs — a miss would
    /// silently render nothing at all.
    #[test]
    fn bravura_glyphs_shape() {
        let glyphs = load_glyphs();
        assert_eq!(glyphs.len(), SMUFL.len());

        for (i, g) in glyphs.iter().enumerate() {
            assert!(
                g.width > 1.0,
                "glyph {i} ({:?}) shaped to nothing — is Bravura registered?",
                SMUFL[i]
            );
            assert!(g.baseline > 0.0, "glyph {i} has no baseline");
        }

        // A notehead is about 1.18 staff spaces wide, and the G clef is much
        // wider than it. If these are wildly off, the font that got picked is
        // not Bravura.
        let head = glyphs[G_BLACK].width;
        assert!(
            (head - STAFF_SPACE * 1.18).abs() < STAFF_SPACE * 0.35,
            "noteheadBlack is {head}px, expected about {}px",
            STAFF_SPACE * 1.18
        );
        assert!(glyphs[G_CLEF_G].width > head);
    }

    /// The strip must leave the HUD alone: streak and score sit at the left
    /// of the screen, the mode buttons at the right.
    #[test]
    fn the_strip_keeps_to_the_middle() {
        let band = Band::new(1000.0);
        assert_eq!(band.left, 250.0);
        assert_eq!(band.right, 750.0);
        assert_eq!(band.centre, 500.0, "the playhead is dead centre");
    }

    #[test]
    fn music_fades_out_at_both_ends() {
        let band = Band::new(1000.0);
        assert_eq!(band.fade(band.right), 0.0);
        assert_eq!(band.fade(band.notes_left), 0.0, "dissolves into the clef");
        assert_eq!(band.fade(band.centre), 1.0, "full strength at the playhead");
    }

    #[test]
    fn now_is_at_the_playhead() {
        let band = Band::new(1000.0);
        assert_eq!(band.x_of(12.0, 12.0), band.centre);
        assert!(band.x_of(13.0, 12.0) > band.centre, "the future is to the right");
        assert!(band.x_of(11.0, 12.0) < band.centre, "the past is to the left");
    }
}
