//! Microphone passthrough: what the mic hears, mixed into the piano so you can
//! sing along with it.
//!
//! This captures from the system's default input and hands the samples to the
//! synth, which adds them to its own output. It does **not** open a speaker
//! stream of its own, and that is the whole point.
//!
//! It did, once, and on the machine this was written for that stream was
//! inaudible. Not quietly wrong — measurably present and completely silent. It
//! opened without error, appeared in the audio graph, linked to the right sink,
//! and its content showed up in that sink's monitor at full level; a native
//! client playing the identical tone at the identical amplitude to the identical
//! sink was plainly audible at the same moment, and the app's own synth was
//! audible too. Every layer that could be inspected said the audio was there.
//! It could not be heard. Rather than keep hunting a fault that no measurement
//! would show, the passthrough now rides the one stream that is known to reach
//! the speakers: if you can hear the piano, you can hear yourself, because they
//! are the same samples in the same buffer.
//!
//! The cost is that there is nothing to hear until the synth is playing — no
//! monitoring from the menu, and none at all if the output is a MIDI device
//! rather than the built-in synth. The level meter still moves in the menu,
//! which is what you actually need there: proof the microphone is heard before
//! you start.
//!
//! Capture and the synth run off two clocks that are close but never identical,
//! so they cannot hand samples straight to each other. Capture writes into a
//! lock-free ring and the synth drains it; the ring absorbs the difference.
//! [`PREFILL`] samples of head start mean a late read still finds something
//! waiting, and the [`MAX_FILL`] ceiling stops a fast microphone quietly growing
//! the delay between singing and hearing yourself.
//!
//! The microphone's own dial sets how much signal arrives, as it should; there
//! is a fixed [`MAKEUP_GAIN`] stage on top to bring a voice up to where the
//! synth runs, and a limiter after it so a shout flattens instead of squaring
//! off into a crunch. Not a volume control: nothing to set, and the dial still
//! decides what goes in.
//!
//! One thing to know before concluding this is broken, because it cost a day:
//! **on speakers you will struggle to hear your own voice through it.** The
//! round trip is on the order of fifteen milliseconds, which is far too short to
//! arrive as an echo — it fuses with the sound of your own head and reads as
//! your voice being a little fuller. Your live voice is far louder at your own
//! ears than the speakers are and masks the rest. Every other sound comes back
//! obviously; your own speech does not. Tap the microphone or whisper and it is
//! unmistakable, and on headphones the problem disappears entirely. This is a
//! property of monitoring, not a fault, and no amount of gain fixes it — the
//! level was measured at the speakers, arriving at full scale, while it was
//! being reported as complete silence. Hence the line the menu prints under the
//! row when it is switched on: headphones are the tool for this job, and saying
//! so up front costs a sentence and saves the day it cost here.
//!
//! It is not a PA either, which is the other way to expect this to behave. A PA
//! is audible over the performer's own voice because its speakers are much
//! louder than they are and are pointed away from the microphone. Here the
//! output is already at full scale by the time it leaves — whether it can beat
//! your live voice is a question about how loud the speakers in the room go,
//! and nothing in this file can raise that ceiling.

use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicUsize, Ordering},
};

use cpal::{
    Sample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

/// Frames per callback to ask the capture device for.
///
/// Half what the synth asks for. The synth's buffer is the delay between
/// pressing a key and hearing it and is already as tight as it can safely go;
/// this one only ever holds a voice, so it can be shorter without putting the
/// piano at risk of a dropout. At 48kHz this is under three milliseconds.
const TARGET_BUFFER_FRAMES: u32 = 128;

/// Samples of head start the synth waits for before it starts draining, and
/// again after any underrun. Two capture callbacks' worth, so one late one
/// costs nothing.
const PREFILL: usize = 256;

/// How far ahead capture is allowed to get. Without a ceiling, a capture clock
/// even slightly faster than playback fills the ring and stays full, and the
/// delay settles at whatever the ring holds rather than at [`PREFILL`]. Samples
/// past this are dropped: a sample lost every few seconds is inaudible, and
/// twenty milliseconds of latency you can hear.
const MAX_FILL: usize = 1024;

/// Ring capacity. A power of two so the wrap is a mask, and comfortably above
/// [`MAX_FILL`], which is what actually bounds the fill.
const RING_CAPACITY: usize = 8192;

/// How much louder the microphone is made on its way through.
///
/// A voice arrives at a USB mic well below where the synth runs, so some lift is
/// needed to sing over a piano — the chord this was checked against peaks at
/// about -13 dBFS. Four times puts a sensibly-set microphone a little under
/// that, which is where a voice wants to sit next to the instrument rather than
/// over it, and leaves the limiter below with something to do only on the loud
/// notes.
///
/// It cannot be exactly right for every microphone, because how much signal
/// arrives is the dial's job, not this constant's. That is what the meter in
/// the menu is for. One number, easy to tune, if a particular setup wants more.
const MAKEUP_GAIN: f32 = 4.0;

/// Where the limiter starts to bend the signal.
///
/// Below this, samples pass through untouched, so normal singing is exactly
/// what the microphone heard. Above it the curve flattens off towards full
/// scale and never crosses it, which is what turns a shout into a loud shout
/// rather than the crunch of a squared-off waveform. Chosen to meet the
/// straight part with the same slope, so there is no audible corner where the
/// two join.
const LIMIT_KNEE: f32 = 0.7;

/// How much of the level reading survives each callback, so a peak falls back
/// visibly instead of sticking at the loudest thing that ever happened. Around
/// twenty decibels a second at the buffer sizes above: slow enough to read off,
/// quick enough to follow a voice.
const METER_DECAY: f32 = 0.988;

/// A single-producer, single-consumer queue of samples shared by the two audio
/// callbacks.
///
/// Samples are held as their bit patterns in atomics, which buys a lock-free
/// queue with no unsafe. Neither callback may block, and a mutex held by one
/// while the other is late is exactly the stall that turns into a click.
struct Ring {
    slots: Box<[AtomicU32]>,
    /// Samples pushed and popped in total, ever. Capture alone writes
    /// `written`, playback alone writes `read`; each publishes its own with
    /// `Release` and reads the other's with `Acquire`, which is what makes the
    /// sample stores visible across the two threads.
    written: AtomicUsize,
    read: AtomicUsize,
}

impl Ring {
    fn new() -> Self {
        Self {
            slots: (0..RING_CAPACITY).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }

    /// Samples waiting. Playback's to ask: it owns `read`, so nothing can move
    /// the answer under it. Saturating because the other end does move, and a
    /// count read a moment stale is fine while a panic is not.
    fn len(&self) -> usize {
        self.written
            .load(Ordering::Acquire)
            .saturating_sub(self.read.load(Ordering::Acquire))
    }

    /// Capture side only.
    fn push(&self, sample: f32) {
        let written = self.written.load(Ordering::Relaxed);
        if written.saturating_sub(self.read.load(Ordering::Acquire)) >= MAX_FILL {
            return;
        }

        self.slots[written % RING_CAPACITY].store(sample.to_bits(), Ordering::Relaxed);
        self.written.store(written + 1, Ordering::Release);
    }

    /// Reading side only. `None` means the ring ran dry.
    fn pop(&self) -> Option<f32> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.written.load(Ordering::Acquire) {
            return None;
        }

        let sample = f32::from_bits(self.slots[read % RING_CAPACITY].load(Ordering::Relaxed));
        self.read.store(read + 1, Ordering::Release);
        Some(sample)
    }

    /// Throw away whatever is waiting. Only safe with the writer stopped, which
    /// is the case when passthrough has just been switched off — dropping the
    /// capture stream joins its thread before this runs.
    fn clear(&self) {
        self.read
            .store(self.written.load(Ordering::Acquire), Ordering::Release);
    }
}

/// What the synth reads the microphone from.
///
/// One of these lives for the whole session and is shared with whatever output
/// stream the synth currently has, so switching soundfont or output device does
/// not need to know anything about microphones. It yields silence whenever
/// passthrough is off, which is what makes [`Monitor::next`] safe to call
/// unconditionally from the audio callback — no branch, no flag to check.
pub struct Monitor {
    ring: Ring,
    /// Reader-side state: hold silence until [`PREFILL`] has built up, and again
    /// from any underrun until it has built up once more. Waiting out a gap is
    /// quieter than chasing the capture stream sample by sample. An atomic
    /// because it is shared by reference, though only the reader touches it.
    primed: std::sync::atomic::AtomicBool,
}

impl Monitor {
    fn new() -> Self {
        Self {
            ring: Ring::new(),
            primed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The next microphone sample to add to the output, or zero when there is
    /// nothing to add. Called once per frame from the synth's audio callback.
    pub fn next(&self) -> f32 {
        if !self.primed.load(Ordering::Relaxed) {
            if self.ring.len() < PREFILL {
                return 0.0;
            }
            self.primed.store(true, Ordering::Relaxed);
        }

        match self.ring.pop() {
            Some(sample) => sample,
            None => {
                self.primed.store(false, Ordering::Relaxed);
                0.0
            }
        }
    }

    /// Drop anything still queued, so switching passthrough off stops the sound
    /// at once rather than trickling out the last few milliseconds of it.
    fn reset(&self) {
        self.ring.clear();
        self.primed.store(false, Ordering::Relaxed);
    }
}

/// The audio host and the capture device, found once and then kept for the life
/// of the app — the same thing `SynthBackend` does with its own.
///
/// Not rebuilt per toggle, and that matters: cpal's ALSA host keeps a
/// process-wide context that tears down ALSA's global configuration when the
/// last one goes, so a host created and dropped around every switch-on is a
/// standing invitation for the second one to misbehave. Finding the device once
/// takes that question off the table.
///
/// Holding it does not pin a particular microphone: it names the system default,
/// and which hardware that is gets resolved when a stream is opened, so each
/// switch-on still picks up whatever the default is by then.
struct Devices {
    _host: cpal::Host,
    capture: cpal::Device,
    /// Never opened here. Kept only to ask what rate the synth's stream runs at,
    /// since that is the rate captured samples have to arrive at to be added to
    /// it — and asking the same device the same way is how the two stay in step.
    playback: Option<cpal::Device>,
}

impl Devices {
    fn open() -> Result<Self, String> {
        let host = cpal::default_host();

        let capture = host
            .default_input_device()
            .ok_or_else(|| "no input device".to_string())?;
        let playback = host.default_output_device();

        Ok(Self {
            _host: host,
            capture,
            playback,
        })
    }

    /// The synth's sample rate, or the common default if the device will not
    /// say — a rate that is wrong by a little only shifts the pitch by a little,
    /// which beats refusing to run at all.
    fn synth_rate(&self) -> u32 {
        self.playback
            .as_ref()
            .and_then(|device| device.default_output_config().ok())
            .map(|config| config.sample_rate())
            .unwrap_or(48_000)
    }
}

/// The capture stream, alive for exactly as long as passthrough is on: dropping
/// it closes the device, which is all "off" means — the microphone is released
/// rather than held open in the background.
struct Live {
    _capture: cpal::Stream,
    /// The loudest thing the microphone has sent lately, as f32 bits: written
    /// by the capture callback, read by the menu. See [`MicPassthrough::level`].
    level: Arc<AtomicU32>,
}

/// Microphone passthrough, off until asked. Session-lived on purpose — nothing
/// is written to the config, so a launch never starts with an open mic the
/// player has forgotten about.
pub struct MicPassthrough {
    /// Found on the first switch-on and kept from then on, however many times
    /// it is toggled after that. See [`Devices`].
    devices: Option<Devices>,
    live: Option<Live>,
    /// Where captured samples go. Handed to the synth once at startup and shared
    /// for the rest of the session, so nothing downstream has to be rebuilt when
    /// passthrough is toggled — it simply stops being fed.
    monitor: Arc<Monitor>,
    /// Why the last attempt to switch it on failed, so the menu can say so
    /// rather than looking like the click did nothing.
    error: Option<String>,
}

impl Default for MicPassthrough {
    fn default() -> Self {
        Self {
            devices: None,
            live: None,
            monitor: Arc::new(Monitor::new()),
            error: None,
        }
    }
}

impl MicPassthrough {
    pub fn is_on(&self) -> bool {
        self.live.is_some()
    }

    /// The shared queue the synth adds to its output. Handed over once, at
    /// startup, and valid whether or not passthrough is ever switched on.
    pub fn monitor(&self) -> Arc<Monitor> {
        Arc::clone(&self.monitor)
    }

    /// Set when switching on failed, cleared by anything that succeeds.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The peak the microphone is sending, 0 to 1, decaying between peaks.
    /// Zero while passthrough is off.
    ///
    /// Read straight off the microphone, before the gain, because setting the
    /// dial is what it is for: a bar that moves when you speak says the mic is
    /// heard, and a bar jammed at the top says the dial is turned up past what
    /// the microphone can take and should come down. It is also the only
    /// feedback you get in the menu, where there is no synth stream to sing
    /// along with yet.
    pub fn level(&self) -> f32 {
        self.live
            .as_ref()
            .map(|live| f32::from_bits(live.level.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn toggle(&mut self) {
        if self.is_on() {
            self.stop();
        } else {
            self.start();
        }
    }

    fn start(&mut self) {
        if self.devices.is_none() {
            match Devices::open() {
                Ok(devices) => self.devices = Some(devices),
                Err(err) => {
                    log::warn!("microphone passthrough unavailable: {err}");
                    self.error = Some(err);
                    return;
                }
            }
        }

        let devices = self.devices.as_ref().expect("just opened above");

        match open_capture(devices, Arc::clone(&self.monitor)) {
            Ok(live) => {
                self.live = Some(live);
                self.error = None;
            }
            Err(err) => {
                log::warn!("microphone passthrough unavailable: {err}");
                self.error = Some(err);
            }
        }
    }

    fn stop(&mut self) {
        // Dropped first: this joins the capture thread, so nothing is still
        // writing by the time the queue is emptied.
        self.live = None;
        self.monitor.reset();
        self.error = None;
    }
}

/// Open the microphone and start feeding the monitor.
fn open_capture(devices: &Devices, monitor: Arc<Monitor>) -> Result<Live, String> {
    let capture_device = &devices.capture;

    let capture_supported = capture_device
        .default_input_config()
        .map_err(|err| format!("input device: {err}"))?;

    let capture_format = capture_supported.sample_format();
    let capture_rate = capture_supported.sample_rate();
    let capture_config = stream_config(capture_supported);

    // The rate the synth will be running at, which is what these samples have
    // to arrive at to be added to its output frame by frame. Asked of the same
    // device and the same way `SynthBackend` asks, so the two agree.
    let synth_rate = devices.synth_rate();

    // Named as well as measured: it follows whatever the system has set as its
    // default, and "which device did it actually pick" is the first question
    // worth answering when nothing can be heard.
    log::info!(
        "microphone passthrough: capture \"{capture_device}\" {capture_rate} Hz {}ch \
         {capture_format:?} -> synth at {synth_rate} Hz, buffer {:?}",
        capture_config.channels,
        capture_config.buffer_size,
    );

    let level = Arc::new(AtomicU32::new(0));

    let capture = capture_stream(
        capture_device,
        capture_config,
        capture_format,
        monitor,
        Arc::clone(&level),
        capture_rate,
        synth_rate,
    )
    .map_err(|err| format!("microphone: {err}"))?;

    capture.play().map_err(|err| format!("microphone: {err}"))?;

    Ok(Live {
        _capture: capture,
        level,
    })
}

/// The device's own configuration, with the buffer pulled down to
/// [`TARGET_BUFFER_FRAMES`] where the backend will say what it accepts — and
/// left alone where it will not, rather than guessing.
fn stream_config(supported: cpal::SupportedStreamConfig) -> cpal::StreamConfig {
    let buffer_size = match supported.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            cpal::BufferSize::Fixed(TARGET_BUFFER_FRAMES.clamp(*min, *max))
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    };

    let mut config: cpal::StreamConfig = supported.into();
    config.buffer_size = buffer_size;
    config
}

#[allow(clippy::too_many_arguments)]
fn capture_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    monitor: Arc<Monitor>,
    level: Arc<AtomicU32>,
    capture_rate: u32,
    synth_rate: u32,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($t:ty) => {
            build_capture::<$t>(device, config, monitor, level, capture_rate, synth_rate)
                .map_err(|err| err.to_string())
        };
    }

    match format {
        cpal::SampleFormat::I8 => build!(i8),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::I64 => build!(i64),
        cpal::SampleFormat::U8 => build!(u8),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::U32 => build!(u32),
        cpal::SampleFormat::U64 => build!(u64),
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::F64 => build!(f64),
        format => Err(format!("unsupported sample format {format}")),
    }
}

fn build_capture<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    monitor: Arc<Monitor>,
    level: Arc<AtomicU32>,
    capture_rate: u32,
    synth_rate: u32,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(capture_rate, synth_rate);

    // Diagnostic, kept because it is what finally settled this: set
    // KEYBOARD_HERO_MIC_TEST_TONE and a steady tone goes where the microphone's
    // samples would, so "can you hear it" answers whether everything downstream
    // of capture works, separately from whether the microphone is loud enough
    // to notice. Those two questions look identical from the outside and cost a
    // day of chasing the wrong one.
    let test_tone = std::env::var_os("KEYBOARD_HERO_MIC_TEST_TONE").is_some();
    let tone_step = 440.0 * std::f32::consts::TAU / capture_rate as f32;
    let mut tone_phase = 0.0f32;

    device.build_input_stream(
        config,
        move |input: &[T], _: &cpal::InputCallbackInfo| {
            let mut peak = 0.0f32;

            for frame in input.chunks(channels) {
                // A microphone is one voice however many channels it arrives
                // on, and the synth adds it to both of its channels, so fold it
                // down here rather than carrying the copies through the ring.
                let sample = frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>()
                    / frame.len().max(1) as f32;

                // Metered before anything is done to it, because what the meter
                // is for is setting the microphone's own dial. After the gain
                // and the limiter every reading crowds the top of the scale and
                // says nothing about whether the dial is right — worse, it
                // hides the one thing worth warning about, which is a
                // microphone already clipping before the app sees it.
                peak = peak.max(sample.abs());

                // Brought up to a level you can hear over a piano, then held
                // under full scale.
                let sample = limit(sample * MAKEUP_GAIN);

                let sample = if test_tone {
                    tone_phase = (tone_phase + tone_step) % std::f32::consts::TAU;
                    tone_phase.sin() * 0.3
                } else {
                    sample
                };

                resampler.feed(sample, |sample| monitor.ring.push(sample));
            }

            // Nothing else writes this, so a plain read-modify-write is safe;
            // the menu only ever reads it.
            let previous = f32::from_bits(level.load(Ordering::Relaxed)) * METER_DECAY;
            level.store(peak.max(previous).to_bits(), Ordering::Relaxed);
        },
        |err| log::warn!("microphone capture: {err}"),
        None,
    )
}

/// Holds a sample under full scale without a hard edge.
///
/// Straight through below [`LIMIT_KNEE`], and beyond it the remaining headroom
/// is spent asymptotically, so however loud the input gets the output only
/// approaches 1.0. The two halves meet with a matching slope, which is what
/// keeps the transition inaudible: a hard clip here would be heard as
/// distortion on exactly the notes a singer leans into.
fn limit(sample: f32) -> f32 {
    let magnitude = sample.abs();
    if magnitude <= LIMIT_KNEE {
        return sample;
    }

    let headroom = 1.0 - LIMIT_KNEE;
    sample.signum() * (LIMIT_KNEE + headroom * ((magnitude - LIMIT_KNEE) / headroom).tanh())
}

/// Turns samples arriving at the capture device's rate into samples at the
/// playback device's, one at a time, so capture can push straight into the ring
/// and playback never has to think about rates at all.
///
/// The two devices usually agree, in which case `step` is exactly 1 and this is
/// a copy. When they do not, `phase` walks between each pair of captured
/// samples, emitting a playback sample every `step` of the way and taking the
/// value between them by straight interpolation — plenty for a voice being
/// monitored, and cheap enough to sit in an audio callback.
///
/// Reading between two samples means holding one back until the next arrives,
/// so everything comes out one captured sample later than it went in. That is
/// twenty microseconds at these rates, next to nothing beside the buffers
/// either side of it, and there is no way around it: the value between two
/// samples is not knowable until both are in hand.
struct Resampler {
    /// How far one playback sample moves along the captured signal.
    step: f64,
    /// The sample before the one being fed; the interpolation reads between
    /// this and it.
    previous: f32,
    /// Where the next playback sample falls between `previous` and the sample
    /// being fed, as a fraction of the gap. Always at least 0.
    phase: f64,
}

impl Resampler {
    fn new(capture_rate: u32, playback_rate: u32) -> Self {
        Self {
            step: capture_rate as f64 / playback_rate.max(1) as f64,
            previous: 0.0,
            phase: 0.0,
        }
    }

    fn feed(&mut self, sample: f32, mut emit: impl FnMut(f32)) {
        while self.phase < 1.0 {
            emit(self.previous + (sample - self.previous) * self.phase as f32);
            self.phase += self.step;
        }

        // Carried over to the next gap, which is what keeps the output rate
        // right across samples rather than only within one.
        self.phase -= 1.0;
        self.previous = sample;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a second of capture turns into, so a pair of rates can be checked
    /// against the count it is supposed to produce.
    fn resample(capture_rate: u32, synth_rate: u32) -> Vec<f32> {
        let mut resampler = Resampler::new(capture_rate, synth_rate);
        let mut out = Vec::new();
        for n in 0..capture_rate {
            resampler.feed(n as f32, |sample| out.push(sample));
        }
        out
    }

    #[test]
    fn matching_rates_pass_samples_through_untouched() {
        let mut resampler = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        for sample in [0.25, -0.5, 0.75] {
            resampler.feed(sample, |sample| out.push(sample));
        }
        // Every sample back exactly as it went in, one place along: see the
        // note on `Resampler` about the sample of delay it works from.
        assert_eq!(out, vec![0.0, 0.25, -0.5]);
    }

    #[test]
    fn mismatched_rates_come_out_at_the_playback_rate() {
        // A second in is a second out, whichever way the rates run. One sample
        // of slack: where the last one lands depends on the leftover phase.
        for (capture, playback) in [(44_100, 48_000), (48_000, 44_100), (96_000, 48_000)] {
            let produced = resample(capture, playback).len() as i64;
            assert!(
                (produced - playback as i64).abs() <= 1,
                "{capture} Hz -> {playback} Hz produced {produced}, wanted {playback}",
            );
        }
    }

    #[test]
    fn resampled_output_stays_within_the_signal_it_came_from() {
        // Interpolating between neighbours cannot overshoot them, so a ramp
        // comes out a ramp: nothing clipped, nothing ringing.
        let out = resample(44_100, 48_000);
        assert!(
            out.windows(2).all(|pair| pair[0] <= pair[1]),
            "a rising ramp came out unsorted"
        );
        assert!(out.iter().all(|sample| (0.0..44_100.0).contains(sample)));
    }

    #[test]
    fn the_limiter_leaves_ordinary_singing_alone() {
        for sample in [0.0, 0.1, -0.25, 0.5, LIMIT_KNEE, -LIMIT_KNEE] {
            assert_eq!(limit(sample), sample, "{sample} should pass untouched");
        }
    }

    #[test]
    fn the_limiter_never_lets_anything_past_full_scale() {
        // However hard it is driven, and symmetrically, so the waveform is not
        // bent out of shape in one direction only. Full scale itself is fine —
        // the curve approaches it and, once the ratio is large enough for the
        // tangent to round to one, sits exactly on it. What must never happen
        // is going past, which is what wraps round into a crack.
        for sample in [0.8, 1.0, 2.0, 50.0, 1e6] {
            assert!(limit(sample) <= 1.0, "{sample} escaped");
            assert!(limit(sample) > LIMIT_KNEE, "{sample} was crushed");
            assert_eq!(limit(-sample), -limit(sample));
        }
    }

    #[test]
    fn the_limiter_has_no_corner_where_it_takes_over() {
        // A step at the knee would be heard as distortion the moment a singer
        // leans in, so check the two halves meet and keep the same slope.
        let step = 1e-4;
        let below = (limit(LIMIT_KNEE) - limit(LIMIT_KNEE - step)) / step;
        let above = (limit(LIMIT_KNEE + step) - limit(LIMIT_KNEE)) / step;
        assert!(
            (below - above).abs() < 0.01,
            "slope jumped: {below} -> {above}"
        );
    }

    #[test]
    fn a_voice_lands_in_the_same_range_as_the_piano() {
        // The piano this has to sit beside peaks around -13 dBFS, measured off
        // the synth's own stream. A microphone with its dial set sensibly sends
        // speech averaging about -22 dBFS, and the job of the gain is to put
        // that in the same neighbourhood: loud enough to be part of the music,
        // not so loud it becomes the whole of it.
        let dialled_in_average = 10f32.powf(-22.0 / 20.0);
        let out_db = 20.0 * limit(dialled_in_average * MAKEUP_GAIN).log10();
        assert!(
            (-20.0..-3.0).contains(&out_db),
            "a voice lands at {out_db:.1} dBFS, nowhere near the piano at -13"
        );

        // The peaks that come with it are held, never clipped: speech runs
        // roughly sixteen decibels above its average, which at this gain is
        // over the top and has to be caught rather than wrapped.
        let dialled_in_peak = 10f32.powf(-6.0 / 20.0);
        assert!(
            limit(dialled_in_peak * MAKEUP_GAIN) <= 1.0,
            "past full scale"
        );
    }

    #[test]
    fn the_monitor_is_silent_until_it_has_a_head_start() {
        let monitor = Monitor::new();

        // Nothing captured yet: the synth must get exact zeros, or switching
        // passthrough on would tick.
        assert_eq!(monitor.next(), 0.0);

        // Still short of the head start, so still silent.
        for _ in 0..PREFILL - 1 {
            monitor.ring.push(0.5);
        }
        assert_eq!(monitor.next(), 0.0);

        // One more and it starts, from the oldest sample.
        monitor.ring.push(0.5);
        assert_eq!(monitor.next(), 0.5);
    }

    #[test]
    fn the_monitor_goes_quiet_again_when_it_runs_dry() {
        let monitor = Monitor::new();
        for _ in 0..PREFILL {
            monitor.ring.push(0.25);
        }

        for _ in 0..PREFILL {
            assert_eq!(monitor.next(), 0.25);
        }

        // Drained. Silence rather than anything stale, and it waits for a fresh
        // head start rather than chasing the microphone sample by sample.
        assert_eq!(monitor.next(), 0.0);
        monitor.ring.push(0.25);
        assert_eq!(monitor.next(), 0.0, "should not restart on a single sample");
    }

    #[test]
    fn switching_off_drops_what_was_still_queued() {
        let monitor = Monitor::new();
        for _ in 0..PREFILL {
            monitor.ring.push(0.75);
        }
        assert_eq!(monitor.next(), 0.75);

        monitor.reset();

        // Nothing trickles out after the microphone is released.
        assert_eq!(monitor.next(), 0.0);
        assert_eq!(monitor.ring.len(), 0);
    }

    #[test]
    fn ring_hands_samples_over_in_order() {
        let ring = Ring::new();
        assert_eq!(ring.pop(), None);

        for n in 0..100 {
            ring.push(n as f32);
        }
        assert_eq!(ring.len(), 100);

        for n in 0..100 {
            assert_eq!(ring.pop(), Some(n as f32));
        }
        assert_eq!(ring.pop(), None);
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn ring_drops_samples_rather_than_letting_the_delay_grow() {
        let ring = Ring::new();
        for n in 0..RING_CAPACITY * 2 {
            ring.push(n as f32);
        }

        assert_eq!(ring.len(), MAX_FILL);
        // What it kept is the oldest: playback is behind, and what it has yet
        // to play is what should come out next.
        assert_eq!(ring.pop(), Some(0.0));
    }

    #[test]
    fn ring_keeps_its_place_across_a_wrap() {
        let ring = Ring::new();
        // Well past the point where the counters fold back around the slots.
        for n in 0..RING_CAPACITY * 3 {
            ring.push(n as f32);
            assert_eq!(ring.pop(), Some(n as f32));
        }
        assert_eq!(ring.len(), 0);
    }
}
