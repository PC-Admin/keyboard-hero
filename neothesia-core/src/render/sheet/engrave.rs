//! Turns a MIDI note list into something that can be *drawn as notation*.
//!
//! This is deliberately not a full engraver. The sheet bar lays notes out
//! proportionally in time (like the waterfall does vertically), so all the
//! genuinely hard parts of engraving — measure widths, beaming, collision
//! avoidance, system breaks — simply don't arise. What is left is the part
//! that actually teaches sheet reading:
//!
//! * which line or space a pitch sits on (diatonic spelling, key-signature
//!   aware, so a G major song shows F# rather than "F, but sharp"),
//! * which notehead/flag a duration gets,
//! * treble vs bass and the ledger lines in between.
//!
//! Everything here is pure and unit-tested; the drawing lives in the parent
//! module.

use midi_file::MidiNote;
use std::time::Duration;

/// Semitone above C for each of the seven note letters.
const LETTER_SEMITONE: [i32; 7] = [0, 2, 4, 5, 7, 9, 11];

/// Letters that pick up a sharp, in the order key signatures add them:
/// F C G D A E B.
const SHARP_ORDER: [usize; 7] = [3, 0, 4, 1, 5, 2, 6];
/// Letters that pick up a flat: B E A D G C F.
const FLAT_ORDER: [usize; 7] = [6, 2, 5, 1, 4, 0, 3];

/// Staff positions of the key-signature accidentals on a treble staff, in the
/// order they are written. Bass positions are these minus two octaves.
const SHARP_SIG_STEPS: [i32; 7] = [10, 7, 11, 8, 5, 9, 6];
const FLAT_SIG_STEPS: [i32; 7] = [6, 9, 5, 8, 4, 7, 3];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accidental {
    Flat,
    Natural,
    Sharp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    Whole,
    Half,
    Black,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staff {
    Treble,
    Bass,
}

impl Staff {
    /// Staff position of the middle line — the one that flips stem direction.
    pub fn middle_line_step(self) -> i32 {
        match self {
            // B4 on the treble staff, D3 on the bass staff.
            Staff::Treble => 6,
            Staff::Bass => -6,
        }
    }

    /// Positions of the five printed lines, low to high.
    pub fn line_steps(self) -> [i32; 5] {
        match self {
            Staff::Treble => [2, 4, 6, 8, 10],
            Staff::Bass => [-10, -8, -6, -4, -2],
        }
    }
}

/// Which staves the bar draws for this song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staves {
    Treble,
    Bass,
    Grand,
}

impl Staves {
    pub fn contains(self, staff: Staff) -> bool {
        matches!(
            (self, staff),
            (Staves::Grand, _) | (Staves::Treble, Staff::Treble) | (Staves::Bass, Staff::Bass)
        )
    }

    /// Highest and lowest printed staff line, as staff positions.
    pub fn extent(self) -> (i32, i32) {
        match self {
            Staves::Treble => (10, 2),
            Staves::Bass => (-2, -10),
            Staves::Grand => (10, -10),
        }
    }

    /// Least room that must be left clear above and below the printed lines,
    /// whatever the song does. A G clef curls almost three staff spaces above
    /// the top line, so the treble staff can never have less than six
    /// positions of headroom.
    pub fn min_padding_steps(self) -> (i32, i32) {
        match self {
            Staves::Treble => (6, 2),
            Staves::Bass => (2, 6),
            Staves::Grand => (6, 6),
        }
    }
}

/// One notehead, already placed on the staff.
#[derive(Debug, Clone, Copy)]
pub struct SheetNote {
    pub start: f32,
    pub end: f32,
    /// Diatonic staff position: 0 is middle C, +1 per letter upwards. Even
    /// numbers land on treble lines, so the maths stays the same for both
    /// staves.
    pub step: i32,
    pub accidental: Option<Accidental>,
    pub head: Head,
    pub dotted: bool,
    pub staff: Staff,
    /// Noteheads a second apart can't share a stem side, so the lower one of
    /// the pair is nudged right by one notehead width.
    pub head_shift: bool,
}

/// The stem shared by every notehead struck at the same instant on one staff.
#[derive(Debug, Clone, Copy)]
pub struct SheetStem {
    pub start: f32,
    pub staff: Staff,
    pub down: bool,
    /// Position of the notehead the stem grows out of.
    pub base_step: i32,
    /// Where the stem ends, in staff positions (fractional: stems are 3.5
    /// spaces = 7 positions long).
    pub tip_step: f32,
    /// Set when the attached notehead is the shifted one of a second.
    pub base_shifted: bool,
    /// 0 = quarter or longer, 1 = quaver, 2 = semiquaver.
    pub flags: u8,
}

/// A whole song, engraved.
#[derive(Debug, Clone, Default)]
pub struct Score {
    pub notes: Vec<SheetNote>,
    pub stems: Vec<SheetStem>,
    /// Key signature as a count of sharps (positive) or flats (negative).
    pub fifths: i32,
    pub staves: Staves,
    /// Bar lines, in seconds.
    pub barlines: Vec<f32>,
    /// Highest and lowest staff position anything in the song reaches, stem
    /// tips included. The bar is sized to these so a treble-only melody
    /// doesn't reserve half its height for a bass staff that never appears.
    pub top_step: i32,
    pub bottom_step: i32,
}

impl Default for Staves {
    fn default() -> Self {
        Staves::Grand
    }
}

impl Score {
    /// Where the key-signature accidentals go on the given staff, and which
    /// symbol to draw, in writing order.
    pub fn key_signature(&self, staff: Staff) -> Vec<(i32, Accidental)> {
        let shift = if staff == Staff::Treble { 0 } else { -14 };
        if self.fifths > 0 {
            SHARP_SIG_STEPS
                .iter()
                .take(self.fifths as usize)
                .map(|s| (s + shift, Accidental::Sharp))
                .collect()
        } else {
            FLAT_SIG_STEPS
                .iter()
                .take((-self.fifths) as usize)
                .map(|s| (s + shift, Accidental::Flat))
                .collect()
        }
    }
}

/// Alteration each letter carries under a key signature of `fifths`.
fn key_alterations(fifths: i32) -> [i32; 7] {
    let mut alter = [0; 7];
    if fifths > 0 {
        for &l in SHARP_ORDER.iter().take(fifths.min(7) as usize) {
            alter[l] = 1;
        }
    } else {
        for &l in FLAT_ORDER.iter().take((-fifths).min(7) as usize) {
            alter[l] = -1;
        }
    }
    alter
}

/// Pick the key signature that spells the song with the fewest accidentals.
///
/// A pitch-class histogram is enough here: the winner is the signature whose
/// scale covers the most notes actually played, and ties go to the simpler
/// signature so a run of chromatic passing notes can't drag a C major tune
/// off to five sharps.
pub fn detect_fifths(pitches: impl Iterator<Item = u8>) -> i32 {
    let mut hist = [0u32; 12];
    for p in pitches {
        hist[(p % 12) as usize] += 1;
    }

    let mut best = 0;
    let mut best_score = i64::MIN;

    for fifths in -7..=7i32 {
        let alter = key_alterations(fifths);
        let mut in_key = [false; 12];
        for l in 0..7 {
            in_key[(LETTER_SEMITONE[l] + alter[l]).rem_euclid(12) as usize] = true;
        }

        let covered: i64 = (0..12)
            .filter(|pc| in_key[*pc])
            .map(|pc| hist[pc] as i64)
            .sum();

        // Nudge towards fewer accidentals so a near-tie resolves to the
        // plainer key.
        let score = covered * 100 - fifths.abs() as i64;
        if score > best_score {
            best_score = score;
            best = fifths;
        }
    }

    best
}

/// Spell one MIDI pitch under a key signature.
///
/// Returns the staff position (0 = middle C) and the accidental that has to be
/// *printed* — which is `None` whenever the key signature already says it.
pub fn spell(midi: u8, alter: &[i32; 7]) -> (i32, Option<Accidental>) {
    let pc = (midi as i32).rem_euclid(12);

    // The key signature already covers this pitch: no printed accidental.
    for letter in 0..7 {
        if (LETTER_SEMITONE[letter] + alter[letter]).rem_euclid(12) == pc {
            return (step_of(midi, alter[letter], letter), None);
        }
    }

    // Outside the key. Cancelling the key signature comes first — an F
    // natural in G major is an F with a natural sign, never an E sharp — and
    // only then the alteration the key leans towards.
    let lean = if alter.iter().sum::<i32>() >= 0 { 1 } else { -1 };
    let order: [i32; 3] = [0, lean, -lean];

    for a in order {
        for letter in 0..7 {
            if alter[letter] == a {
                // Would print nothing, but we already know it doesn't match.
                continue;
            }
            if (LETTER_SEMITONE[letter] + a).rem_euclid(12) == pc {
                let printed = match a {
                    1 => Accidental::Sharp,
                    -1 => Accidental::Flat,
                    _ => Accidental::Natural,
                };
                return (step_of(midi, a, letter), Some(printed));
            }
        }
    }

    // Unreachable for 12-tone input, but fall back to a plain C-major spelling
    // rather than panicking on a malformed file.
    (step_of(midi, 0, 0), None)
}

/// Staff position of `midi` when spelled as `letter` altered by `a`.
///
/// Backing the alteration out first is what keeps B#3 and Cb4 on the right
/// side of the octave line.
fn step_of(midi: u8, a: i32, letter: usize) -> i32 {
    let natural = midi as i32 - a;
    let octave = natural.div_euclid(12) - 1;
    (octave - 4) * 7 + letter as i32
}

/// Length of a quarter note at `t`, in seconds.
///
/// Read off the measure grid rather than the tempo map, so tempo changes and
/// ritardandos are followed for free. `quarters_per_bar` comes from the file's
/// time signature: three for a 3/4 minuet, so its bars aren't mistaken for
/// slow 4/4 ones.
fn quarter_at(measures: &[Duration], quarters_per_bar: f32, t: f32) -> f32 {
    if measures.len() < 2 {
        return 0.5;
    }

    let idx = measures
        .partition_point(|m| m.as_secs_f32() <= t)
        .saturating_sub(1)
        .min(measures.len() - 2);

    let bar = measures[idx + 1].as_secs_f32() - measures[idx].as_secs_f32();
    if bar > 0.01 {
        bar / quarters_per_bar.max(1.0)
    } else {
        0.5
    }
}

/// Notated value for a sounded duration, in quarter notes.
///
/// MIDI files are performances: a "quarter note" is usually held for 80-95% of
/// its written length, and a staccato one for a third of it. Snapping the raw
/// duration would show a song of semiquavers, so the measured length is
/// stretched a little first and then matched to the nearest written value on a
/// log scale (where a dotted quaver really does sit midway between a quaver
/// and a crotchet).
fn notated_value(beats: f32) -> (Head, bool, u8) {
    const VALUES: [(f32, Head, bool, u8); 9] = [
        (0.25, Head::Black, false, 2),  // semiquaver
        (0.375, Head::Black, true, 2),  // dotted semiquaver
        (0.5, Head::Black, false, 1),   // quaver
        (0.75, Head::Black, true, 1),   // dotted quaver
        (1.0, Head::Black, false, 0),   // crotchet
        (1.5, Head::Black, true, 0),    // dotted crotchet
        (2.0, Head::Half, false, 0),    // minim
        (3.0, Head::Half, true, 0),     // dotted minim
        (4.0, Head::Whole, false, 0),   // semibreve
    ];

    let stretched = (beats * 1.12).clamp(0.16, 6.0);

    let mut best = VALUES[4];
    let mut best_err = f32::INFINITY;
    for v in VALUES {
        let err = (stretched.ln() - v.0.ln()).abs();
        if err < best_err {
            best_err = err;
            best = v;
        }
    }

    (best.1, best.2, best.3)
}

/// Engrave a set of MIDI notes.
///
/// `measures` is the bar grid from [`midi_file::MidiFile`] and supplies the
/// bar lines; together with `quarters_per_bar` from the file's time signature
/// it also gives the beat length used to choose noteheads.
pub fn engrave(notes: &[MidiNote], measures: &[Duration], quarters_per_bar: f32) -> Score {
    let mut melodic: Vec<&MidiNote> = notes.iter().filter(|n| n.channel != 9).collect();
    melodic.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| a.note.cmp(&b.note))
    });

    if melodic.is_empty() {
        return Score::default();
    }

    let fifths = detect_fifths(melodic.iter().map(|n| n.note));
    let alter = key_alterations(fifths);

    // Only print the staff a song actually uses: a right-hand melody gets a
    // single treble staff instead of an empty bass one taking up half the bar.
    let below = melodic.iter().filter(|n| n.note < 60).count();
    let above = melodic.len() - below;
    let staves = if below * 8 < melodic.len() {
        Staves::Treble
    } else if above * 8 < melodic.len() {
        Staves::Bass
    } else {
        Staves::Grand
    };

    let staff_for = |midi: u8| -> Staff {
        match staves {
            Staves::Treble => Staff::Treble,
            Staves::Bass => Staff::Bass,
            Staves::Grand => {
                if midi >= 60 {
                    Staff::Treble
                } else {
                    Staff::Bass
                }
            }
        }
    };

    let mut sheet_notes: Vec<SheetNote> = Vec::with_capacity(melodic.len());
    for n in &melodic {
        let start = n.start.as_secs_f32();
        let beats = n.duration.as_secs_f32() / quarter_at(measures, quarters_per_bar, start).max(0.01);
        // Flags belong to the stem, which is shared across a chord, so only
        // the head and the dot are kept here.
        let (head, dotted, _) = notated_value(beats);
        let (step, accidental) = spell(n.note, &alter);

        sheet_notes.push(SheetNote {
            start,
            end: (n.start + n.duration).as_secs_f32(),
            step,
            accidental,
            head,
            dotted,
            staff: staff_for(n.note),
            head_shift: false,
        });
    }

    // Group simultaneous notes on a staff into chords: they share one stem,
    // and seconds inside them need their noteheads offsetting.
    let mut stems: Vec<SheetStem> = Vec::new();
    let mut i = 0;
    while i < sheet_notes.len() {
        // Notes within ~30ms of each other were meant as one chord.
        let start = sheet_notes[i].start;
        let mut j = i;
        while j < sheet_notes.len() && sheet_notes[j].start - start < 0.03 {
            j += 1;
        }

        for staff in [Staff::Treble, Staff::Bass] {
            let mut idx: Vec<usize> = (i..j).filter(|k| sheet_notes[*k].staff == staff).collect();
            if idx.is_empty() {
                continue;
            }
            idx.sort_by_key(|k| sheet_notes[*k].step);

            // Offset the upper note of any second so the heads don't collide.
            // Only one of a pair may move, or a cluster would walk off to the
            // right a notehead at a time.
            let mut prev: Option<i32> = None;
            let mut prev_shifted = false;
            for &k in &idx {
                let step = sheet_notes[k].step;
                let shift = prev == Some(step - 1) && !prev_shifted;
                sheet_notes[k].head_shift = shift;
                prev_shifted = shift;
                prev = Some(step);
            }

            let low = sheet_notes[idx[0]];
            let high = sheet_notes[*idx.last().unwrap()];

            // Whole notes have no stem at all.
            if low.head == Head::Whole {
                continue;
            }

            let beats = (low.end - low.start) / quarter_at(measures, quarters_per_bar, start).max(0.01);
            let (_, _, flags) = notated_value(beats);

            // The note furthest from the middle line decides which way the
            // stem points, as it does in real engraving.
            let mid = staff.middle_line_step();
            let down = (high.step - mid).abs() >= (mid - low.step).abs();

            let (base, base_shifted, tip) = if down {
                (high.step, high.head_shift, low.step as f32 - 7.0)
            } else {
                (low.step, low.head_shift, high.step as f32 + 7.0)
            };

            stems.push(SheetStem {
                start,
                staff,
                down,
                base_step: base,
                tip_step: tip,
                base_shifted,
                flags,
            });
        }

        i = j;
    }

    let last = sheet_notes.last().map(|n| n.end).unwrap_or(0.0);
    let barlines = measures
        .iter()
        .map(|m| m.as_secs_f32())
        .take_while(|t| *t <= last + 4.0)
        .collect();

    let (mut top_step, mut bottom_step) = (i32::MIN, i32::MAX);
    for n in &sheet_notes {
        top_step = top_step.max(n.step);
        bottom_step = bottom_step.min(n.step);
    }
    for s in &stems {
        top_step = top_step.max(s.tip_step.ceil() as i32);
        bottom_step = bottom_step.min(s.tip_step.floor() as i32);
    }

    Score {
        notes: sheet_notes,
        stems,
        fifths,
        staves,
        barlines,
        top_step,
        bottom_step,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(midi: u8, start_ms: u64, dur_ms: u64) -> MidiNote {
        MidiNote {
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(start_ms + dur_ms),
            duration: Duration::from_millis(dur_ms),
            note: midi,
            velocity: 100,
            channel: 0,
            track_id: 0,
            track_color_id: 0,
        }
    }

    /// 120bpm 4/4: a bar every two seconds.
    fn measures(n: usize) -> Vec<Duration> {
        (0..n).map(|i| Duration::from_secs(i as u64 * 2)).collect()
    }

    #[test]
    fn middle_c_is_step_zero() {
        let alter = key_alterations(0);
        assert_eq!(spell(60, &alter), (0, None));
        // C5 is an octave up: seven staff positions.
        assert_eq!(spell(72, &alter).0, 7);
        // C3 is an octave down.
        assert_eq!(spell(48, &alter).0, -7);
    }

    #[test]
    fn treble_and_bass_lines_land_where_they_should() {
        let alter = key_alterations(0);
        // E4, the bottom line of the treble staff.
        assert_eq!(spell(64, &alter).0, 2);
        // F5, the top line.
        assert_eq!(spell(77, &alter).0, 10);
        // G2, the bottom line of the bass staff.
        assert_eq!(spell(43, &alter).0, -10);
        // A3, the top line.
        assert_eq!(spell(57, &alter).0, -2);
    }

    #[test]
    fn c_major_prints_sharps_for_black_keys() {
        let alter = key_alterations(0);
        let (step, acc) = spell(61, &alter); // C#4
        assert_eq!(step, 0);
        assert_eq!(acc, Some(Accidental::Sharp));
    }

    #[test]
    fn key_signature_absorbs_its_own_accidentals() {
        // G major: one sharp, on F.
        let alter = key_alterations(1);
        let (step, acc) = spell(66, &alter); // F#4
        assert_eq!(step, 3, "F#4 sits on the F line, not somewhere else");
        assert_eq!(acc, None, "the key signature already says F is sharp");

        // ...and an F natural in G major has to be spelled out.
        let (step, acc) = spell(65, &alter); // F natural
        assert_eq!(step, 3);
        assert_eq!(acc, Some(Accidental::Natural));
    }

    #[test]
    fn flat_keys_spell_with_flats() {
        // Eb major: three flats.
        let alter = key_alterations(-3);
        let (step, acc) = spell(63, &alter); // Eb4
        assert_eq!(step, 2, "Eb sits on the E line");
        assert_eq!(acc, None);

        // A note outside the key leans flat, not sharp.
        let (_, acc) = spell(66, &alter); // Gb4 / F#4
        assert_eq!(acc, Some(Accidental::Flat));
    }

    #[test]
    fn octave_survives_b_sharp() {
        // B#3 (midi 60 spelled as B#) must stay in octave 3, not jump to 4.
        // Seven sharps puts B# in the key.
        let alter = key_alterations(7);
        let (step, acc) = spell(60, &alter);
        assert_eq!(acc, None);
        assert_eq!(step, -1, "B#3 is one position below middle C, not on it");
    }

    #[test]
    fn detects_g_major() {
        // A G major scale.
        let notes: Vec<_> = [67, 69, 71, 72, 74, 76, 78, 79]
            .iter()
            .enumerate()
            .map(|(i, m)| note(*m, i as u64 * 500, 450))
            .collect();
        assert_eq!(detect_fifths(notes.iter().map(|n| n.note)), 1);
    }

    #[test]
    fn detects_c_major_over_a_flashier_neighbour() {
        let notes: Vec<_> = [60, 62, 64, 65, 67, 69, 71, 72]
            .iter()
            .enumerate()
            .map(|(i, m)| note(*m, i as u64 * 500, 450))
            .collect();
        assert_eq!(detect_fifths(notes.iter().map(|n| n.note)), 0);
    }

    #[test]
    fn durations_become_the_right_noteheads() {
        let m = measures(8);
        // At 120bpm a crotchet is 500ms; MIDI usually holds it for ~450ms.
        let notes = vec![
            note(60, 0, 450),   // crotchet
            note(62, 500, 220), // quaver
            note(64, 1000, 900), // minim
            note(65, 2000, 1900), // semibreve
        ];
        let score = engrave(&notes, &m, 4.0);
        assert_eq!(score.notes[0].head, Head::Black);
        assert!(!score.notes[0].dotted);
        assert_eq!(score.notes[1].head, Head::Black);
        assert_eq!(score.notes[2].head, Head::Half);
        assert_eq!(score.notes[3].head, Head::Whole);
    }

    #[test]
    fn quavers_get_a_flag_and_semibreves_get_no_stem() {
        let m = measures(8);
        let score = engrave(&[note(72, 0, 220)], &m, 4.0);
        assert_eq!(score.stems.len(), 1);
        assert_eq!(score.stems[0].flags, 1);

        let score = engrave(&[note(72, 0, 1900)], &m, 4.0);
        assert!(score.stems.is_empty(), "semibreves are stemless");
    }

    #[test]
    fn a_chord_shares_one_stem_and_offsets_its_seconds() {
        let m = measures(8);
        // C-D-E struck together: C/D are a second apart, D/E are too, but
        // only one of the pair may move.
        let score = engrave(&[note(60, 0, 450), note(62, 0, 450), note(64, 0, 450)], &m, 4.0);
        assert_eq!(score.stems.len(), 1, "one stem for the whole chord");

        let shifted: Vec<bool> = score.notes.iter().map(|n| n.head_shift).collect();
        assert_eq!(shifted, vec![false, true, false]);
    }

    #[test]
    fn hands_split_across_the_grand_staff() {
        let m = measures(8);
        let notes = vec![
            note(40, 0, 450),
            note(45, 0, 450),
            note(72, 0, 450),
            note(76, 0, 450),
        ];
        let score = engrave(&notes, &m, 4.0);
        assert_eq!(score.staves, Staves::Grand);
        assert_eq!(score.stems.len(), 2, "one stem per hand");
    }

    #[test]
    fn a_right_hand_melody_gets_a_treble_staff_only() {
        let m = measures(8);
        let notes: Vec<_> = (0..16)
            .map(|i| note(72 + (i % 5) as u8, i as u64 * 250, 220))
            .collect();
        assert_eq!(engrave(&notes, &m, 4.0).staves, Staves::Treble);
    }

    #[test]
    fn key_signature_is_written_in_the_usual_order() {
        let score = Score {
            fifths: 2,
            ..Default::default()
        };
        let sig = score.key_signature(Staff::Treble);
        // F# then C#, at their conventional heights.
        assert_eq!(sig[0], (10, Accidental::Sharp));
        assert_eq!(sig[1], (7, Accidental::Sharp));

        // The bass staff writes the same accidentals two octaves lower.
        let bass = score.key_signature(Staff::Bass);
        assert_eq!(bass[0].0, 10 - 14);
    }

    #[test]
    fn drums_are_left_out() {
        let m = measures(8);
        let mut drum = note(38, 0, 100);
        drum.channel = 9;
        let score = engrave(&[drum, note(60, 0, 450)], &m, 4.0);
        assert_eq!(score.notes.len(), 1);
        assert_eq!(score.notes[0].step, 0, "the surviving note is middle C");
    }

    #[test]
    fn follows_a_tempo_change() {
        // Bars of 2s then 1s — 120bpm, then twice that. The same 220ms of
        // sound is a quaver in the slow bar and a crotchet in the fast one,
        // which is the whole reason the beat length is read off the measure
        // grid rather than assumed.
        let m = vec![
            Duration::from_millis(0),
            Duration::from_millis(2000),
            Duration::from_millis(3000),
            Duration::from_millis(4000),
        ];
        let score = engrave(&[note(60, 0, 220), note(62, 2000, 220)], &m, 4.0);
        assert_eq!(score.stems[0].flags, 1, "quaver in the slow bar");
        assert_eq!(score.stems[1].flags, 0, "crotchet in the fast bar");
    }
}
