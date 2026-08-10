//! The spectrum analyser in the top-right corner: a rainbow of bars showing
//! what is coming out of the speakers, right now.
//!
//! It listens to one thing — the synth's own output stream — and that is
//! deliberate, because by the time a sample reaches that stream it is already
//! everything you can hear: the piano, and the microphone too when passthrough
//! is on, since [`crate::microphone`] mixes singing into the same buffer rather
//! than opening a stream of its own. One tap, both sources, and no way for the
//! picture to disagree with the sound. The alternative — capturing the
//! microphone a second time just to draw it — would open the input device
//! behind the player's back and still double-count the voice the synth is
//! already carrying.
//!
//! What follows from that is worth knowing before concluding it is broken: with
//! passthrough off, singing moves nothing, because nothing you sing is in the
//! output. And with a MIDI keyboard chosen as the output device instead of the
//! built-in synth, the analyser stays at its resting line however loudly the
//! room is playing — the notes are going out a MIDI cable to an instrument this
//! process never hears. The VOICE and PIANO readouts underneath say which of
//! those is happening rather than leaving a flat display to explain itself.
//!
//! The audio callback may not block, so the tap is a ring of atomics that the
//! callback only ever writes to and the frame loop only ever reads from, with
//! no coordination between them at all: a reader that catches the writer
//! mid-lap gets one window with a seam in it, which is a frame of slightly
//! wrong bar heights sixteen milliseconds before the next one replaces it.
//! That is the right trade here and it would not be in [`crate::microphone`],
//! where the same seam would be a click.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use neothesia_core::render::{QuadInstance, QuadRenderer};

/// Samples the tap keeps. A power of two so the wrap is a mask, and several
/// times [`WINDOW`] so a frame that arrives late still finds a whole window
/// behind the write head rather than reading into samples it already drew.
const CAPACITY: usize = 8192;
const MASK: usize = CAPACITY - 1;

/// Samples per analysis window: about 43ms at 48kHz, and 23Hz apart in the
/// result. Shorter loses the bass — the lowest bar here is 45Hz, and you cannot
/// see a frequency you have not watched for a whole cycle of. Longer smears the
/// attack of a note across frames, which is exactly the thing the display is
/// for.
const WINDOW: usize = 2048;

/// Bars across the display. Sized against the panel it is drawn in: at the
/// width of the performer selector these come out around three pixels each,
/// fine enough to show the shape of a chord rather than a handful of blocks.
pub const BANDS: usize = 48;

/// Lowest and highest frequency a bar stands for. The bottom is under the piano
/// (A0 is 27.5Hz, but its fundamental is barely there and everything below 40Hz
/// is room rumble), the top is where a voice's sibilance lives and hearing
/// gives out shortly after.
const LOW_HZ: f32 = 45.0;
const HIGH_HZ: f32 = 16_000.0;

/// The loudness window mapped onto the height of a bar. Below the floor a bar
/// sits on the baseline; at the ceiling it is full height. Chosen so ordinary
/// playing uses most of the height and the room's noise floor does not lift the
/// display off its rest.
const FLOOR_DB: f32 = -76.0;
const CEIL_DB: f32 = -8.0;

/// How much the treble is lifted, in decibels per octave above [`TILT_PIVOT_HZ`].
///
/// Music has far less energy up high than down low — bass fundamentals carry
/// most of it — so an untilted analyser is a wall on the left and a flat line
/// on the right. Every analyser worth looking at tilts, and this is the usual
/// amount: enough that a cymbal or a consonant reaches for the top of the
/// display, not so much that hiss starts drawing itself.
const TILT_DB_PER_OCTAVE: f32 = 4.0;
const TILT_PIVOT_HZ: f32 = 180.0;
/// Ceiling on that lift, so the very top bars cannot run away.
const TILT_MAX_DB: f32 = 20.0;

/// How quickly a bar rises to a new reading, and how slowly it falls back.
/// Rise is near-instant because the strike of a note is the thing worth seeing;
/// fall is slow enough to read but quick enough that a stopped chord does not
/// hang around.
const ATTACK_TAU: f32 = 0.012;
const DECAY_TAU: f32 = 0.16;

/// How long a peak cap hangs at the height it was left, and how hard it then
/// accelerates downwards. A held cap is what lets you see how loud a note *was*
/// after its bar has already dropped.
const PEAK_HOLD: f32 = 0.55;
const PEAK_FALL: f32 = 0.9;

/// Degrees of hue swept across the display: red in the bass through to violet
/// at the top. Stopping short of a full circle keeps the highest bar from
/// coming back round to the red the lowest one already has.
const RAINBOW_SWEEP: f32 = 285.0;

/// Height the panel needs, including the row of readouts along the bottom.
pub const PANEL_HEIGHT: f32 = 84.0;
/// Where that row starts, measured from the top of the panel.
pub const CAPTION_TOP: f32 = 68.0;

/// Padding inside the panel.
const PAD: f32 = 8.0;
/// Room under the baseline for the bars' reflections.
const REFLECTION: f32 = 10.0;

/// Where the analyser reads its audio from.
///
/// Written by whichever audio callback is playing, read by the frame loop, and
/// shared by [`std::sync::Arc`] between them. Samples are held as their bit
/// patterns in atomics, which buys the sharing with no lock and no unsafe.
pub struct SpectrumTap {
    slots: Box<[AtomicU32]>,
    /// Samples written in total, ever. The write position is this modulo the
    /// ring; published with `Release` so the sample stores land first.
    written: AtomicUsize,
    /// Rate the writer is running at, or zero before anything has been written.
    /// Needed to turn a bin number into a frequency, and it is the stream that
    /// knows it.
    sample_rate: AtomicU32,
}

impl Default for SpectrumTap {
    fn default() -> Self {
        Self::new()
    }
}

impl SpectrumTap {
    pub fn new() -> Self {
        Self {
            slots: (0..CAPACITY).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicUsize::new(0),
            sample_rate: AtomicU32::new(0),
        }
    }

    /// Told once, when a stream is opened.
    pub fn set_sample_rate(&self, hz: u32) {
        self.sample_rate.store(hz, Ordering::Relaxed);
    }

    /// Audio-callback side: one mono sample of whatever is being played.
    ///
    /// Two relaxed stores and an add, with nothing to wait on — safe to call
    /// once per frame from inside a callback that must never block.
    pub fn push(&self, sample: f32) {
        let written = self.written.load(Ordering::Relaxed);
        self.slots[written & MASK].store(sample.to_bits(), Ordering::Relaxed);
        self.written
            .store(written.wrapping_add(1), Ordering::Release);
    }

    /// Frame-loop side: the most recent `out.len()` samples, oldest first.
    ///
    /// `None` until a whole window has been played and the writer has said what
    /// rate it is running at — there is nothing honest to draw before that.
    fn read_latest(&self, out: &mut [f32]) -> Option<Reading> {
        debug_assert!(out.len() <= CAPACITY);

        let sample_rate = self.sample_rate.load(Ordering::Relaxed);
        let written = self.written.load(Ordering::Acquire);

        if sample_rate == 0 || written < out.len() {
            return None;
        }

        let start = written - out.len();
        for (offset, sample) in out.iter_mut().enumerate() {
            *sample = f32::from_bits(self.slots[(start + offset) & MASK].load(Ordering::Relaxed));
        }

        Some(Reading {
            sample_rate: sample_rate as f32,
            written,
        })
    }
}

struct Reading {
    sample_rate: f32,
    written: usize,
}

/// One bar: where it is now, and where its cap is.
#[derive(Clone, Default)]
struct Bar {
    level: f32,
    peak: f32,
    peak_hold: f32,
    /// Speed the cap is falling at, which builds while it falls.
    peak_fall: f32,
}

/// The analyser proper: turns a tap into bar heights, and draws them.
pub struct Analyzer {
    fft: Fft,
    /// Hann window, applied before the transform. Without it, a note that does
    /// not fit a whole number of times into the window smears itself across
    /// every bin and the display turns to mush.
    window: Vec<f32>,
    frame: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    /// Linear magnitude per bin, up to the Nyquist frequency.
    magnitude: Vec<f32>,
    /// Hz per bin, from the rate the tap is running at. Zero until it has been
    /// heard from.
    bin_hz: f32,
    /// Worked out once: what each bar covers, and its share of the treble lift.
    edge_hz: Vec<(f32, f32)>,
    centre_hz: Vec<f32>,
    tilt_db: Vec<f32>,
    bars: Vec<Bar>,
    /// Loudness of the whole mix, smoothed, for the readout underneath.
    level: f32,
    /// What the tap's counter said last frame. A stream that has stopped — the
    /// output switched to a MIDI device, say — leaves its last samples sitting
    /// in the ring, and drawing those forever would freeze the display at
    /// whatever was playing when it stopped.
    last_written: usize,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        let window = (0..WINDOW)
            .map(|n| {
                let phase = std::f32::consts::TAU * n as f32 / WINDOW as f32;
                0.5 - 0.5 * phase.cos()
            })
            .collect();

        // Bars are spaced by ratio, not by hertz, because that is how pitch
        // works: every bar is the same musical distance wide as the one beside
        // it, so an octave takes up the same room wherever it sits.
        let ratio = (HIGH_HZ / LOW_HZ).powf(1.0 / BANDS as f32);
        let edge_hz: Vec<(f32, f32)> = (0..BANDS)
            .map(|band| {
                let low = LOW_HZ * ratio.powi(band as i32);
                (low, low * ratio)
            })
            .collect();
        let centre_hz: Vec<f32> = edge_hz
            .iter()
            .map(|(low, high)| (low * high).sqrt())
            .collect();
        let tilt_db = centre_hz
            .iter()
            .map(|hz| (TILT_DB_PER_OCTAVE * (hz / TILT_PIVOT_HZ).log2()).clamp(0.0, TILT_MAX_DB))
            .collect();

        Self {
            fft: Fft::new(WINDOW),
            window,
            frame: vec![0.0; WINDOW],
            re: vec![0.0; WINDOW],
            im: vec![0.0; WINDOW],
            magnitude: vec![0.0; WINDOW / 2],
            bin_hz: 0.0,
            edge_hz,
            centre_hz,
            tilt_db,
            bars: vec![Bar::default(); BANDS],
            level: 0.0,
            last_written: 0,
        }
    }

    /// How loud the mix is overall, 0 to 1 — what the PIANO readout brightens
    /// with.
    pub fn level(&self) -> f32 {
        self.level
    }

    /// Read the tap and move every bar towards what it says.
    ///
    /// `delta` is clamped, because the first frame of a song covers all of
    /// startup and a drag or an alt-tab covers however long it lasted; left
    /// alone, either would collapse the whole display to the baseline in one
    /// step and then climb back out of it in front of the player.
    pub fn update(&mut self, delta: f32, tap: &SpectrumTap) {
        let delta = delta.clamp(0.0, 1.0 / 20.0);

        self.sample(tap);

        let mut loudest = 0.0f32;
        for band in 0..BANDS {
            let magnitude = self.band_magnitude(band);

            // A decibel scale, because loudness is what the eye should be
            // reading: on a linear one everything but the loudest note or two
            // sits invisibly close to the floor.
            let db = 20.0 * (magnitude + 1e-9).log10() + self.tilt_db[band];
            let target = ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0);
            loudest = loudest.max(target);

            let bar = &mut self.bars[band];
            let tau = if target > bar.level {
                ATTACK_TAU
            } else {
                DECAY_TAU
            };
            bar.level += (target - bar.level) * (1.0 - (-delta / tau).exp());

            if bar.level >= bar.peak {
                bar.peak = bar.level;
                bar.peak_hold = PEAK_HOLD;
                bar.peak_fall = 0.0;
            } else if bar.peak_hold > 0.0 {
                bar.peak_hold -= delta;
            } else {
                bar.peak_fall += PEAK_FALL * delta;
                bar.peak = (bar.peak - bar.peak_fall * delta).max(bar.level);
            }
        }

        self.level += (loudest - self.level) * (1.0 - (-delta / 0.09).exp());
    }

    /// Pull a window off the tap and turn it into magnitudes. Leaves silence
    /// behind whenever there is nothing new to look at.
    fn sample(&mut self, tap: &SpectrumTap) {
        let Some(reading) = tap.read_latest(&mut self.frame) else {
            self.magnitude.fill(0.0);
            return;
        };

        if reading.written == self.last_written {
            self.magnitude.fill(0.0);
            return;
        }
        self.last_written = reading.written;
        self.bin_hz = reading.sample_rate / WINDOW as f32;

        for (n, sample) in self.frame.iter().enumerate() {
            self.re[n] = sample * self.window[n];
            self.im[n] = 0.0;
        }

        self.fft.transform(&mut self.re, &mut self.im);

        // Two for the half of the spectrum being thrown away, and another two
        // for the half the Hann window took, so a full-scale sine tone comes
        // back out at 1.0 and the decibel scale above means what it says.
        let scale = 4.0 / WINDOW as f32;
        for (bin, magnitude) in self.magnitude.iter_mut().enumerate() {
            *magnitude = (self.re[bin] * self.re[bin] + self.im[bin] * self.im[bin]).sqrt() * scale;
        }
    }

    /// The strength of one bar's slice of the spectrum.
    fn band_magnitude(&self, band: usize) -> f32 {
        if self.bin_hz <= 0.0 {
            return 0.0;
        }

        let bins = self.magnitude.len();
        let (low, high) = self.edge_hz[band];

        // Bin zero is DC — a constant offset in the signal rather than a sound —
        // so no bar ever reads it, and the last bin is where this rate runs out.
        let first = ((low / self.bin_hz).ceil() as usize).max(1);
        let last = ((high / self.bin_hz).floor() as usize).min(bins - 1);

        // The loudest bin it covers, rather than the average: a single strong
        // note sharing a bar with quiet neighbours is a tall bar, which is what
        // it sounds like.
        if first <= last {
            return self.magnitude[first..=last]
                .iter()
                .copied()
                .fold(0.0, f32::max);
        }

        // Narrower than the transform can resolve, which is every bar down in
        // the bass: read between the two bins it falls between instead, so the
        // bottom of the display is a curve rather than a staircase of bars all
        // showing the same bin.
        let centre = self.centre_hz[band] / self.bin_hz;
        let lower = centre.floor() as usize;
        if lower < 1 || lower + 1 >= bins {
            // Below the first usable bin, or above what this rate can carry.
            return 0.0;
        }

        let fraction = centre - lower as f32;
        self.magnitude[lower] * (1.0 - fraction) + self.magnitude[lower + 1] * fraction
    }

    /// Draw the panel into `(x, y)`..`(x + width, y + height)`.
    ///
    /// Every bar is four or five quads: a bloom behind it, the bar, a hot tip
    /// where it ends, a reflection under the baseline, and the cap. Together
    /// that is what stops it looking like a bar chart.
    pub fn render(&self, quads: &mut QuadRenderer, x: f32, y: f32, width: f32, height: f32) {
        let inner_x = x + PAD;
        let inner_w = width - PAD * 2.0;
        if inner_w <= 0.0 {
            return;
        }

        let ceiling = y + PAD;
        let baseline = y + CAPTION_TOP - REFLECTION - 2.0;
        let bar_max = baseline - ceiling;
        if bar_max <= 0.0 {
            return;
        }

        // Backdrop, so the bars read against the waterfall behind them.
        quad(
            quads,
            x,
            y,
            width,
            height,
            [0.05, 0.05, 0.07],
            0.45,
            [10.0; 4],
        );
        // The line they stand on, which is also what a silent analyser shows.
        quad(
            quads,
            inner_x,
            baseline,
            inner_w,
            1.0,
            [1.0, 1.0, 1.0],
            0.10,
            [0.0; 4],
        );

        let slot = inner_w / BANDS as f32;
        let bar_w = (slot * 0.72).max(1.0);

        for (band, bar) in self.bars.iter().enumerate() {
            let across = band as f32 / (BANDS - 1) as f32;
            let hue = across * RAINBOW_SWEEP;
            let level = bar.level.clamp(0.0, 1.0);

            // Never quite nothing: at rest the bars are a thin rainbow sitting
            // on the baseline, which says the analyser is alive and listening.
            let bar_h = (level * bar_max).max(1.5);
            let bar_x = inner_x + band as f32 * slot + (slot - bar_w) * 0.5;
            let top = baseline - bar_h;

            let colour = hsv(hue, 0.88 - 0.30 * level, 0.50 + 0.50 * level);
            let radius = (bar_w * 0.5).min(bar_h * 0.5);

            // Bloom: a wider, fainter copy underneath, so a loud bar glows into
            // the ones beside it instead of ending at its own edge.
            quad(
                quads,
                bar_x - 1.2,
                top - 1.6,
                bar_w + 2.4,
                bar_h + 1.6,
                colour,
                0.08 + 0.20 * level,
                [bar_w; 4],
            );

            quad(
                quads,
                bar_x,
                top,
                bar_w,
                bar_h,
                colour,
                0.95,
                [radius, radius, 0.0, 0.0],
            );

            // The tip runs hot: the same hue washed out towards white, which is
            // what makes a bar look lit rather than painted.
            let tip = bar_h.min(3.5);
            quad(
                quads,
                bar_x,
                top,
                bar_w,
                tip,
                hsv(hue, 0.25, 1.0),
                0.35 + 0.55 * level,
                [radius; 4],
            );

            // Reflection below the line, as if the bars were standing on glass.
            let mirror = (bar_h * 0.30).min(REFLECTION);
            quad(
                quads,
                bar_x,
                baseline + 2.0,
                bar_w,
                mirror,
                colour,
                0.14,
                [0.0, 0.0, radius, radius],
            );

            if bar.peak > 0.02 {
                let cap_y = baseline - bar.peak.clamp(0.0, 1.0) * bar_max - 3.0;
                quad(
                    quads,
                    bar_x,
                    cap_y,
                    bar_w,
                    2.0,
                    hsv(hue, 0.20, 1.0),
                    0.55,
                    [1.0; 4],
                );
            }
        }
    }
}

/// One rounded rectangle. Everything the analyser draws is made of these.
#[allow(clippy::too_many_arguments)]
fn quad(
    quads: &mut QuadRenderer,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    colour: [f32; 3],
    alpha: f32,
    border_radius: [f32; 4],
) {
    quads.push(QuadInstance {
        position: [x, y],
        size: [width, height],
        color: [colour[0], colour[1], colour[2], alpha],
        border_radius,
    });
}

/// Hue in degrees, saturation and value 0 to 1, out to RGB.
///
/// A rainbow is a sweep of hue and nothing else, which is the one thing plain
/// RGB cannot express — mixing between named colours gives muddy browns where
/// this gives the spectrum.
fn hsv(hue: f32, saturation: f32, value: f32) -> [f32; 3] {
    let hue = hue.rem_euclid(360.0) / 60.0;
    let sector = hue.floor();
    let offset = hue - sector;

    let p = value * (1.0 - saturation);
    let q = value * (1.0 - saturation * offset);
    let t = value * (1.0 - saturation * (1.0 - offset));

    match sector as u32 {
        0 => [value, t, p],
        1 => [q, value, p],
        2 => [p, value, t],
        3 => [p, q, value],
        4 => [t, p, value],
        _ => [value, p, q],
    }
}

/// A radix-2 fast Fourier transform, with its twiddle factors and its
/// bit-reversal worked out once at startup.
///
/// Hand-rolled rather than pulled in, because this is the whole of what the
/// analyser needs from a transform: one size, one direction, real input.
struct Fft {
    /// Where each position moves to in the bit-reversal shuffle the algorithm
    /// starts from.
    reversed: Vec<u32>,
    /// exp(-2πi k/n) for k up to n/2, which is every twiddle any stage needs —
    /// a stage of length `len` reads every `n / len`th of them.
    twiddle_re: Vec<f32>,
    twiddle_im: Vec<f32>,
}

impl Fft {
    fn new(n: usize) -> Self {
        assert!(
            n >= 2 && n.is_power_of_two(),
            "window must be a power of two"
        );

        let bits = n.trailing_zeros();
        let reversed = (0..n as u32)
            .map(|i| i.reverse_bits() >> (32 - bits))
            .collect();

        let mut twiddle_re = Vec::with_capacity(n / 2);
        let mut twiddle_im = Vec::with_capacity(n / 2);
        for k in 0..n / 2 {
            let angle = -std::f32::consts::TAU * k as f32 / n as f32;
            twiddle_re.push(angle.cos());
            twiddle_im.push(angle.sin());
        }

        Self {
            reversed,
            twiddle_re,
            twiddle_im,
        }
    }

    /// In place, and only right for the length this was built for.
    fn transform(&self, re: &mut [f32], im: &mut [f32]) {
        let n = re.len();
        debug_assert_eq!(n, im.len());
        debug_assert_eq!(n, self.reversed.len());

        for i in 0..n {
            let j = self.reversed[i] as usize;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }

        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let stride = n / len;

            let mut base = 0;
            while base < n {
                for k in 0..half {
                    let twiddle_re = self.twiddle_re[k * stride];
                    let twiddle_im = self.twiddle_im[k * stride];

                    let (ar, ai) = (re[base + k], im[base + k]);
                    let (br, bi) = (re[base + k + half], im[base + k + half]);

                    let tr = br * twiddle_re - bi * twiddle_im;
                    let ti = br * twiddle_im + bi * twiddle_re;

                    re[base + k] = ar + tr;
                    im[base + k] = ai + ti;
                    re[base + k + half] = ar - tr;
                    im[base + k + half] = ai - ti;
                }
                base += len;
            }

            len <<= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed the tap a tone and hand back the analyser that has read it.
    fn analyzer_hearing(hz: f32, amplitude: f32, sample_rate: u32) -> Analyzer {
        let tap = SpectrumTap::new();
        tap.set_sample_rate(sample_rate);

        let step = std::f32::consts::TAU * hz / sample_rate as f32;
        for n in 0..WINDOW {
            tap.push((step * n as f32).sin() * amplitude);
        }

        let mut analyzer = Analyzer::new();
        // One long step, so every bar has reached what it was given rather than
        // being caught partway there.
        analyzer.update(1.0, &tap);
        analyzer
    }

    /// Which bar came out tallest.
    fn loudest_band(analyzer: &Analyzer) -> usize {
        analyzer
            .bars
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.level.total_cmp(&b.1.level))
            .map(|(band, _)| band)
            .expect("there is always at least one bar")
    }

    #[test]
    fn a_tone_lights_the_bar_it_belongs_to() {
        // Within a couple of bars of the right one, rather than exactly the
        // right one: a tone sitting near a boundary spreads either side of it
        // no matter how the transform is done, so demanding the exact bar would
        // be a test of which side of a line the tone was picked on. Landing
        // somewhere else entirely is the mistake worth catching, and two bars
        // out of forty-eight across eight octaves is tight enough to catch it.
        let bar_width = (HIGH_HZ / LOW_HZ).powf(1.0 / BANDS as f32);
        let tolerance = bar_width.powi(2);

        for hz in [220.0, 440.0, 1000.0, 4000.0] {
            let analyzer = analyzer_hearing(hz, 0.5, 48_000);
            let band = loudest_band(&analyzer);
            let centre = analyzer.centre_hz[band];
            let apart = (centre / hz).max(hz / centre);
            assert!(
                apart < tolerance,
                "{hz} Hz lit bar {band}, centred on {centre:.0} Hz"
            );
        }
    }

    #[test]
    fn a_louder_tone_makes_a_taller_bar() {
        let quiet = analyzer_hearing(440.0, 0.05, 48_000);
        let loud = analyzer_hearing(440.0, 0.5, 48_000);

        let band = loudest_band(&loud);
        assert_eq!(band, loudest_band(&quiet), "the same tone moved bars");
        assert!(
            loud.bars[band].level > quiet.bars[band].level + 0.1,
            "ten times the amplitude barely moved the bar"
        );
    }

    #[test]
    fn silence_rests_on_the_baseline() {
        let analyzer = analyzer_hearing(440.0, 0.0, 48_000);
        assert!(
            analyzer.bars.iter().all(|bar| bar.level == 0.0),
            "silence drew something"
        );
        assert_eq!(analyzer.level(), 0.0);
    }

    #[test]
    fn a_stopped_stream_falls_away_rather_than_freezing() {
        let tap = SpectrumTap::new();
        tap.set_sample_rate(48_000);

        let step = std::f32::consts::TAU * 440.0 / 48_000.0;
        for n in 0..WINDOW {
            tap.push((step * n as f32).sin() * 0.5);
        }

        let mut analyzer = Analyzer::new();
        analyzer.update(1.0 / 60.0, &tap);
        let band = loudest_band(&analyzer);
        let while_playing = analyzer.bars[band].level;
        assert!(while_playing > 0.0, "the tone drew nothing");

        // Nothing more is ever pushed: the samples are still sitting in the
        // ring, and reading them again would hold the picture forever.
        for _ in 0..200 {
            analyzer.update(1.0 / 60.0, &tap);
        }
        assert!(
            analyzer.bars[band].level < while_playing * 0.05,
            "the display froze at {while_playing} when the stream stopped"
        );
    }

    #[test]
    fn nothing_is_drawn_before_a_whole_window_has_played() {
        let tap = SpectrumTap::new();
        tap.set_sample_rate(48_000);
        for _ in 0..WINDOW - 1 {
            tap.push(1.0);
        }

        let mut buffer = vec![0.0; WINDOW];
        assert!(tap.read_latest(&mut buffer).is_none());

        tap.push(1.0);
        assert!(tap.read_latest(&mut buffer).is_some());
    }

    #[test]
    fn a_rate_nobody_has_declared_reads_as_nothing() {
        // A tap that has been written to but never told its rate cannot be
        // turned into frequencies, and guessing one would draw the wrong
        // picture rather than none.
        let tap = SpectrumTap::new();
        for _ in 0..WINDOW {
            tap.push(0.5);
        }

        let mut buffer = vec![0.0; WINDOW];
        assert!(tap.read_latest(&mut buffer).is_none());
    }

    #[test]
    fn the_tap_hands_back_the_newest_samples_in_order() {
        let tap = SpectrumTap::new();
        tap.set_sample_rate(48_000);

        // Several laps of the ring, so the wrap is exercised rather than
        // assumed.
        let total = CAPACITY * 3 + 17;
        for n in 0..total {
            tap.push(n as f32);
        }

        let mut buffer = vec![0.0; WINDOW];
        tap.read_latest(&mut buffer).expect("a full window is in");

        let oldest = (total - WINDOW) as f32;
        assert_eq!(buffer[0], oldest);
        assert_eq!(buffer[WINDOW - 1], (total - 1) as f32);
        assert!(
            buffer.windows(2).all(|pair| pair[1] - pair[0] == 1.0),
            "the window came back out of order"
        );
    }

    #[test]
    fn the_bars_climb_the_scale_in_order() {
        // Each bar starts where the one below it ended, covers a fixed musical
        // distance, and the set spans exactly what it claims to.
        let analyzer = Analyzer::new();
        assert_eq!(analyzer.edge_hz.len(), BANDS);
        assert!((analyzer.edge_hz[0].0 - LOW_HZ).abs() < 0.01);
        assert!((analyzer.edge_hz[BANDS - 1].1 - HIGH_HZ).abs() < 1.0);

        for pair in analyzer.edge_hz.windows(2) {
            assert!((pair[0].1 - pair[1].0).abs() < 0.01, "a gap between bars");
        }
    }

    #[test]
    fn the_transform_agrees_with_the_slow_way_of_doing_it() {
        // A small case worked out by the textbook definition, which is the only
        // check that catches a twiddle factor indexed a step wrong.
        const N: usize = 16;
        let input: Vec<f32> = (0..N).map(|n| (n as f32 * 0.7).sin() + 0.3).collect();

        let mut re = input.clone();
        let mut im = vec![0.0; N];
        Fft::new(N).transform(&mut re, &mut im);

        for k in 0..N {
            let (mut want_re, mut want_im) = (0.0f32, 0.0f32);
            for (n, sample) in input.iter().enumerate() {
                let angle = -std::f32::consts::TAU * (k * n) as f32 / N as f32;
                want_re += sample * angle.cos();
                want_im += sample * angle.sin();
            }
            assert!((re[k] - want_re).abs() < 1e-3, "bin {k} real part");
            assert!((im[k] - want_im).abs() < 1e-3, "bin {k} imaginary part");
        }
    }

    #[test]
    fn a_full_scale_tone_reaches_the_top_of_the_display() {
        // The decibel window is anchored to full scale, so the loudest thing an
        // audio stream can carry has to reach the ceiling — otherwise the top
        // of every bar's travel is height nothing ever uses.
        let analyzer = analyzer_hearing(1000.0, 1.0, 48_000);
        let band = loudest_band(&analyzer);
        assert!(
            analyzer.bars[band].level > 0.95,
            "full scale only reached {:.2} of the height",
            analyzer.bars[band].level
        );
    }

    #[test]
    fn the_rainbow_runs_from_red_to_violet_without_wrapping() {
        let first = hsv(0.0, 1.0, 1.0);
        let last = hsv(RAINBOW_SWEEP, 1.0, 1.0);

        assert_eq!(first, [1.0, 0.0, 0.0], "the bass should be red");
        assert!(
            last[2] > last[1] && last[0] > last[1],
            "the treble should be violet, got {last:?}"
        );
        assert!(RAINBOW_SWEEP < 360.0, "the ends would meet");
    }

    #[test]
    fn every_hue_around_the_wheel_is_a_colour() {
        let mut degrees = 0.0;
        while degrees < 360.0 {
            let rgb = hsv(degrees, 1.0, 1.0);
            assert!(
                rgb.iter().all(|c| (0.0..=1.0).contains(c)),
                "{degrees} degrees gave {rgb:?}"
            );
            // Fully saturated and fully bright: one channel is always at the top.
            assert!(
                rgb.iter().copied().fold(0.0, f32::max) > 0.99,
                "{degrees} degrees came out dull: {rgb:?}"
            );
            degrees += 7.5;
        }
    }
}
