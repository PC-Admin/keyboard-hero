//! Microphone passthrough: what the mic hears, played straight back out of the
//! speakers so you can sing over the piano the app is playing.
//!
//! It is a pair of audio streams of its own — capture from the system's default
//! input, playback to its default output — rather than anything routed through
//! the synth. That is the choice the sound effects already made
//! (`playing_scene::sfx`), for the same reasons: it keeps working whichever MIDI
//! output the player picked, external keyboards included, and it can be switched
//! on and off without disturbing a note that is already sounding. Both halves go
//! to the same device the synth uses, so they arrive mixed together.
//!
//! The two streams run off two clocks that are close but never identical, so
//! they cannot hand samples straight to each other. Capture writes into a
//! lock-free ring and playback drains it; the ring absorbs the difference.
//! [`PREFILL`] samples of head start mean a late playback callback still finds
//! something waiting, and the [`MAX_FILL`] ceiling stops a fast microphone
//! quietly growing the delay between singing and hearing yourself.
//!
//! The microphone's own dial sets how much signal arrives, as it should. What
//! it cannot do is make that signal loud enough to sing over a piano: a voice
//! reaches a USB mic at a level tens of decibels below where the synth runs,
//! and passed on untouched it is technically playing and practically inaudible
//! — you hear yourself in the room far louder than a copy that quiet. So there
//! is one fixed [`MAKEUP_GAIN`] stage on the way through, and a limiter after
//! it so that a shout flattens instead of squaring off into a crunch. Not a
//! volume control: nothing to set, and the dial still decides what goes in.

use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicUsize, Ordering},
};

use cpal::{
    Sample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

/// Frames per callback to ask both devices for — the same figure the synth asks
/// for, for the same reason: this is monitoring, so the buffer is delay you can
/// hear. See `output_manager::synth_backend`.
const TARGET_BUFFER_FRAMES: u32 = 256;

/// Samples of head start playback waits for before it begins, and again after
/// any underrun. Two callbacks' worth, so one late one costs nothing.
const PREFILL: usize = 512;

/// How far ahead capture is allowed to get. Without a ceiling, a capture clock
/// even slightly faster than playback fills the ring and stays full, and the
/// delay settles at whatever the ring holds rather than at [`PREFILL`]. Samples
/// past this are dropped: a sample lost every few seconds is inaudible, a tenth
/// of a second of latency is not.
const MAX_FILL: usize = 2048;

/// Ring capacity. A power of two so the wrap is a mask, and comfortably above
/// [`MAX_FILL`], which is what actually bounds the fill.
const RING_CAPACITY: usize = 8192;

/// How much louder the microphone is made on its way through.
///
/// Measured rather than guessed. Speaking into a USB mic whose capture gain is
/// already at maximum lands around -34 dBFS on average, with peaks some twenty
/// decibels above that; the synth and the sound effects sit far higher. Six
/// times over puts an ordinary voice in the same range as the piano it is meant
/// to sing along with. Change this one number to taste — louder is a bigger
/// figure, and the limiter below keeps the top end civil either way.
const MAKEUP_GAIN: f32 = 6.0;

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

    /// Playback side only. `None` means the ring ran dry.
    fn pop(&self) -> Option<f32> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.written.load(Ordering::Acquire) {
            return None;
        }

        let sample = f32::from_bits(self.slots[read % RING_CAPACITY].load(Ordering::Relaxed));
        self.read.store(read + 1, Ordering::Release);
        Some(sample)
    }
}

/// The audio host and the two devices, found once and then kept for the life of
/// the app — the same thing `SynthBackend` does with its own.
///
/// Not rebuilt per toggle, and that matters: cpal's ALSA host keeps a
/// process-wide context that tears down ALSA's global configuration when the
/// last one goes, so a host created and dropped around every switch-on left the
/// second one with a playback stream that reported success, appeared in the
/// audio graph, and carried nothing. Finding the devices once takes that whole
/// question off the table.
///
/// Holding them does not pin a particular microphone: these name the system
/// default, and which hardware that is gets resolved when a stream is opened,
/// so each switch-on still picks up whatever the default is by then.
struct Devices {
    _host: cpal::Host,
    capture: cpal::Device,
    playback: cpal::Device,
}

impl Devices {
    fn open() -> Result<Self, String> {
        let host = cpal::default_host();

        let capture = host
            .default_input_device()
            .ok_or_else(|| "no input device".to_string())?;
        let playback = host
            .default_output_device()
            .ok_or_else(|| "no output device".to_string())?;

        Ok(Self {
            _host: host,
            capture,
            playback,
        })
    }
}

/// The streams, alive for exactly as long as passthrough is on: dropping them
/// closes both, which is all "off" means — the microphone is released rather
/// than held open in the background.
struct Live {
    _capture: cpal::Stream,
    _playback: cpal::Stream,
    /// The loudest thing the microphone has sent lately, as f32 bits: written
    /// by the capture callback, read by the menu. See [`MicPassthrough::level`].
    level: Arc<AtomicU32>,
}

/// Microphone passthrough, off until asked. Session-lived on purpose — nothing
/// is written to the config, so a launch never starts with an open mic the
/// player has forgotten about.
#[derive(Default)]
pub struct MicPassthrough {
    /// Found on the first switch-on and kept from then on, however many times
    /// it is toggled after that. See [`Devices`].
    devices: Option<Devices>,
    live: Option<Live>,
    /// Why the last attempt to switch it on failed, so the menu can say so
    /// rather than looking like the click did nothing.
    error: Option<String>,
}

impl MicPassthrough {
    pub fn is_on(&self) -> bool {
        self.live.is_some()
    }

    /// Set when switching on failed, cleared by anything that succeeds.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The peak going out to the speakers, 0 to 1, decaying between peaks.
    /// Zero while it is off.
    ///
    /// Worth showing because a passthrough that is working and a passthrough
    /// that is silent look identical from the outside — the only difference is
    /// a sound you may well be talking over. Read after the gain and the
    /// limiter, so a bar that moves means audio is leaving the app, and a bar
    /// against the top means the limiter is holding a shout back.
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

        match open_streams(devices) {
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
        self.live = None;
        self.error = None;
    }
}

/// Open a stream on each device and start moving samples between them.
fn open_streams(devices: &Devices) -> Result<Live, String> {
    let capture_device = &devices.capture;
    let playback_device = &devices.playback;

    let capture_supported = capture_device
        .default_input_config()
        .map_err(|err| format!("input device: {err}"))?;
    let playback_supported = playback_device
        .default_output_config()
        .map_err(|err| format!("output device: {err}"))?;

    let capture_format = capture_supported.sample_format();
    let playback_format = playback_supported.sample_format();
    let capture_rate = capture_supported.sample_rate();
    let playback_rate = playback_supported.sample_rate();

    let capture_config = stream_config(capture_supported);
    let playback_config = stream_config(playback_supported);

    // Named as well as measured: both ends follow whatever the system has set
    // as its default, and "which device did it actually pick" is the first
    // question worth answering when nothing can be heard.
    log::info!(
        "microphone passthrough: capture \"{capture_device}\" {capture_rate} Hz {}ch \
         {capture_format:?} -> playback \"{playback_device}\" {playback_rate} Hz {}ch \
         {playback_format:?}, buffer {:?}",
        capture_config.channels,
        playback_config.channels,
        playback_config.buffer_size,
    );

    let ring = Arc::new(Ring::new());
    let level = Arc::new(AtomicU32::new(0));

    let capture = capture_stream(
        capture_device,
        capture_config,
        capture_format,
        Arc::clone(&ring),
        Arc::clone(&level),
        capture_rate,
        playback_rate,
    )
    .map_err(|err| format!("microphone: {err}"))?;

    let playback = playback_stream(playback_device, playback_config, playback_format, ring)
        .map_err(|err| format!("speakers: {err}"))?;

    // Capture first, so the ring is already filling by the time playback looks.
    capture.play().map_err(|err| format!("microphone: {err}"))?;
    playback.play().map_err(|err| format!("speakers: {err}"))?;

    Ok(Live {
        _capture: capture,
        _playback: playback,
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
    ring: Arc<Ring>,
    level: Arc<AtomicU32>,
    capture_rate: u32,
    playback_rate: u32,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($t:ty) => {
            build_capture::<$t>(device, config, ring, level, capture_rate, playback_rate)
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
    ring: Arc<Ring>,
    level: Arc<AtomicU32>,
    capture_rate: u32,
    playback_rate: u32,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    let mut resampler = Resampler::new(capture_rate, playback_rate);

    device.build_input_stream(
        config,
        move |input: &[T], _: &cpal::InputCallbackInfo| {
            let mut peak = 0.0f32;

            for frame in input.chunks(channels) {
                // A microphone is one voice however many channels it arrives
                // on, and playback puts it back out of all of them, so fold it
                // down here rather than carrying the copies through the ring.
                let sample = frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>()
                    / frame.len().max(1) as f32;

                // Brought up to a level you can actually hear, then held under
                // full scale. The meter reads from here, after both, so the bar
                // is what comes out of the speakers rather than what went into
                // the microphone.
                let sample = limit(sample * MAKEUP_GAIN);

                peak = peak.max(sample.abs());
                resampler.feed(sample, |sample| ring.push(sample));
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

fn playback_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    ring: Arc<Ring>,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($t:ty) => {
            build_playback::<$t>(device, config, ring).map_err(|err| err.to_string())
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

fn build_playback<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    ring: Arc<Ring>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;

    // Silent until the ring has its head start, and silent again from an
    // underrun until it has built that up once more. Waiting out a gap is
    // quieter than chasing the capture stream sample by sample.
    let mut primed = false;

    device.build_output_stream(
        config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            if !primed {
                if ring.len() < PREFILL {
                    output.fill(T::EQUILIBRIUM);
                    return;
                }
                primed = true;
            }

            let mut frames = output.chunks_mut(channels);
            for frame in frames.by_ref() {
                let Some(sample) = ring.pop() else {
                    primed = false;
                    frame.fill(T::EQUILIBRIUM);
                    break;
                };
                frame.fill(T::from_sample(sample));
            }

            // Whatever is left of the buffer once the ring ran dry.
            for frame in frames {
                frame.fill(T::EQUILIBRIUM);
            }
        },
        |err| log::warn!("microphone playback: {err}"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a second of capture turns into, so a pair of rates can be checked
    /// against the count it is supposed to produce.
    fn resample(capture_rate: u32, playback_rate: u32) -> Vec<f32> {
        let mut resampler = Resampler::new(capture_rate, playback_rate);
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
    fn a_speaking_voice_ends_up_somewhere_you_can_hear_it() {
        // The measured article: peaks around -15 dBFS off the microphone, which
        // is a healthy input level and still far below the synth. After the
        // makeup stage it should sit near the top of the range without the
        // limiter having to crush it.
        let measured_peak = 10f32.powf(-15.0 / 20.0);
        let out = limit(measured_peak * MAKEUP_GAIN);
        assert!(out > 0.7, "still too quiet to hear: {out}");
        assert!(out < 1.0, "past full scale: {out}");
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
