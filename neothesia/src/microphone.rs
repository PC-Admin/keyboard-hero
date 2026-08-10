//! The microphone: what it hears mixed into the piano so you can sing along,
//! and kept so freeplay can record you doing it.
//!
//! One capture stream serves both, and every sample goes to both — a recording
//! is the audio that went to the speakers rather than a separately processed
//! second version of it, so there is only ever one answer to why a file does
//! not sound like the room. Either half can want the device on its own: you can
//! monitor without recording, and record without monitoring, which is the
//! useful combination on speakers where hearing yourself is not really on offer
//! anyway. The stream opens when the first of them asks and closes when the
//! last stops asking, decided in one place — see [`MicPassthrough::sync_device`].
//!
//! The passthrough half hands samples to the synth, which adds them to its own
//! output. It does **not** open a speaker stream of its own, and that is the
//! whole point.
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
//! The same [`Ring`] does all three jobs here, with different answers to what
//! happens when the reader falls behind. Monitoring drops, because a stale
//! sample is worse than a missing one when the point is hearing yourself now.
//! A recording keeps everything and is sized for the worst frame, because a
//! dropped sample there is a hole in a file somebody saves — and if it ever
//! does fill, the take says so rather than handing over a gap with no
//! explanation. Playing a take back needs no clock at all: the synth drains the
//! queue at exactly the rate it plays, so topping it up once a frame delivers
//! samples at exactly the rate they should be heard, however irregular the
//! frames are.
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

/// Live monitoring ring capacity, comfortably above [`MAX_FILL`], which is what
/// actually bounds the fill.
const RING_CAPACITY: usize = 8192;

/// How much of a take is queued ahead for the synth to play back.
///
/// Nothing here is being monitored, so the tight ceiling that live passthrough
/// needs would only make underruns likely for no gain. Around forty
/// milliseconds: unnoticeable against a recording you are listening back to,
/// and enough that a slow frame does not break the sound.
const PREVIEW_CAPACITY: usize = 4096;

/// How much of a recording may sit waiting to be collected by the frame loop.
///
/// Unlike monitoring, a dropped sample here is a hole in a file somebody keeps,
/// so this is sized for the worst frame rather than the usual one: about two
/// and a half seconds at 48kHz, against the sixteen milliseconds a frame is
/// supposed to take. A stall longer than that is noted rather than hidden.
const TAPE_CAPACITY: usize = 1 << 17;

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

/// A single-producer, single-consumer queue of samples shared by two threads.
///
/// Samples are held as their bit patterns in atomics, which buys a lock-free
/// queue with no unsafe. Neither audio callback may block, and a mutex held by
/// one while the other is late is exactly the stall that turns into a click.
///
/// Capacity and fill ceiling are given per ring rather than fixed, because the
/// three of them in this file want different answers to "what happens when the
/// reader falls behind". Monitoring wants a tight ceiling and is happy to drop:
/// a stale sample is worse than a missing one when the whole point is hearing
/// yourself now. A recording wants the opposite — every sample kept, and enough
/// room that a slow frame cannot cost any.
struct Ring {
    slots: Box<[AtomicU32]>,
    /// Samples pushed and popped in total, ever. The writer alone writes
    /// `written`, the reader alone writes `read`; each publishes its own with
    /// `Release` and reads the other's with `Acquire`, which is what makes the
    /// sample stores visible across the two threads.
    written: AtomicUsize,
    read: AtomicUsize,
    /// Most samples allowed to sit waiting. Pushes past this are dropped rather
    /// than overwriting what has not been read, so a reader that falls behind
    /// loses the newest sound rather than having the oldest torn out from under
    /// it. Never more than `slots.len()`.
    ceiling: usize,
}

impl Ring {
    fn new(capacity: usize, ceiling: usize) -> Self {
        Self {
            slots: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            ceiling: ceiling.min(capacity),
        }
    }

    /// Samples waiting. The reader's to ask: it owns `read`, so nothing can move
    /// the answer under it. Saturating because the other end does move, and a
    /// count read a moment stale is fine while a panic is not.
    fn len(&self) -> usize {
        self.written
            .load(Ordering::Acquire)
            .saturating_sub(self.read.load(Ordering::Acquire))
    }

    /// Writer side only. False when the ring was full and the sample was
    /// dropped, which the recording tape cares about and monitoring does not.
    fn push(&self, sample: f32) -> bool {
        let written = self.written.load(Ordering::Relaxed);
        if written.saturating_sub(self.read.load(Ordering::Acquire)) >= self.ceiling {
            return false;
        }

        self.slots[written % self.slots.len()].store(sample.to_bits(), Ordering::Relaxed);
        self.written.store(written + 1, Ordering::Release);
        true
    }

    /// Reader side only. `None` means the ring ran dry.
    fn pop(&self) -> Option<f32> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.written.load(Ordering::Acquire) {
            return None;
        }

        let sample = f32::from_bits(self.slots[read % self.slots.len()].load(Ordering::Relaxed));
        self.read.store(read + 1, Ordering::Release);
        Some(sample)
    }

    /// Throw away whatever is waiting, and say how much that was — pausing a
    /// preview rewinds by exactly that count, so resuming picks up where the
    /// sound stopped rather than a buffer's worth past it.
    ///
    /// Only safe with the writer stopped, which is the case at every call: the
    /// capture stream is dropped (joining its thread) before the monitor is
    /// cleared, and the preview's writer is the thread doing the clearing.
    fn clear(&self) -> usize {
        let waiting = self.len();
        self.read
            .store(self.written.load(Ordering::Acquire), Ordering::Release);
        waiting
    }
}

/// A ring the synth reads from, with the rule that makes it listenable: stay
/// silent until enough has built up to keep going.
///
/// Without it, the first sample to arrive would be played the instant it landed
/// and the next read would find nothing, so the output would alternate between
/// signal and silence at the callback rate. Waiting for [`PREFILL`] and waiting
/// again after any underrun costs a few milliseconds and turns that into sound.
struct Source {
    ring: Ring,
    /// Reader-side state. An atomic because it is shared by reference, though
    /// only the reader touches it.
    primed: std::sync::atomic::AtomicBool,
}

impl Source {
    fn new(capacity: usize, ceiling: usize) -> Self {
        Self {
            ring: Ring::new(capacity, ceiling),
            primed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The next sample, or zero when there is nothing to give. Called once per
    /// frame from the synth's audio callback.
    fn next(&self) -> f32 {
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

    /// Drop anything still queued and say how much was dropped, so the sound
    /// stops at once rather than trickling out the last few milliseconds.
    fn reset(&self) -> usize {
        self.primed.store(false, Ordering::Relaxed);
        self.ring.clear()
    }
}

/// What the synth reads the microphone from.
///
/// One of these lives for the whole session and is shared with whatever output
/// stream the synth currently has, so switching soundfont or output device does
/// not need to know anything about microphones. It yields silence whenever
/// there is nothing to add, which is what makes [`Monitor::next`] safe to call
/// unconditionally from the audio callback — no branch, no flag to check.
///
/// Two things can want to be heard, and they are summed rather than switched
/// between: the microphone as it is heard now, and a recorded take being played
/// back. Usually only one is running. Both at once is singing along with your
/// own take, which is a reasonable thing to want and costs nothing to allow.
pub struct Monitor {
    /// Fed by the capture callback while passthrough is switched on.
    live: Source,
    /// Whether the live half is wanted. The microphone may be open purely to
    /// record, in which case it is captured and kept but not heard.
    monitoring: std::sync::atomic::AtomicBool,
    /// Fed a frame at a time by [`VoicePlayback`], replaying a finished take.
    preview: Source,
}

impl Monitor {
    fn new() -> Self {
        Self {
            live: Source::new(RING_CAPACITY, MAX_FILL),
            monitoring: std::sync::atomic::AtomicBool::new(false),
            preview: Source::new(PREVIEW_CAPACITY, PREVIEW_CAPACITY),
        }
    }

    /// The next microphone sample to add to the output, or zero when there is
    /// nothing to add. Called once per frame from the synth's audio callback.
    pub fn next(&self) -> f32 {
        self.live.next() + self.preview.next()
    }

    /// Capture side. A no-op unless passthrough is switched on, so recording
    /// with monitoring off puts nothing in the ring to go stale.
    fn push_live(&self, sample: f32) {
        if self.monitoring.load(Ordering::Relaxed) {
            self.live.ring.push(sample);
        }
    }

    /// Start or stop feeding the live half. Switching off empties it, so the
    /// sound stops with the click rather than a buffer later.
    fn set_monitoring(&self, monitoring: bool) {
        self.monitoring.store(monitoring, Ordering::Relaxed);
        if !monitoring {
            self.live.reset();
        }
    }
}

/// Where a recording accumulates between the capture callback, which cannot
/// allocate, and the frame loop, which can.
///
/// The callback pushes into the ring; the frame loop empties it into a `Vec`
/// once a frame. Disarmed it is inert, so the microphone can be open purely for
/// monitoring without quietly filling a buffer nobody is going to read.
struct Tape {
    ring: Ring,
    armed: std::sync::atomic::AtomicBool,
    /// Set if the ring ever filled before the frame loop got back to it, which
    /// means the take has a gap in it. Worth saying rather than handing over a
    /// file with a hole and no explanation.
    dropped: std::sync::atomic::AtomicBool,
}

impl Tape {
    fn new() -> Self {
        Self {
            ring: Ring::new(TAPE_CAPACITY, TAPE_CAPACITY),
            armed: std::sync::atomic::AtomicBool::new(false),
            dropped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Capture side, called for every sample whether armed or not.
    fn push(&self, sample: f32) {
        if self.is_armed() && !self.ring.push(sample) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    /// Start a fresh take. Cleared before arming, so nothing left from the last
    /// one lands at the front of this one.
    fn arm(&self) {
        self.ring.clear();
        self.dropped.store(false, Ordering::Relaxed);
        self.armed.store(true, Ordering::Relaxed);
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::Relaxed);
    }

    /// Move everything captured since last time onto the end of `out`.
    fn collect(&self, out: &mut Vec<f32>) {
        while let Some(sample) = self.ring.pop() {
            out.push(sample);
        }
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

/// The microphone, and the two independent reasons to hold it open: passthrough
/// so you can hear yourself, and recording so a take keeps your voice.
///
/// Either wants the device; neither should close it out from under the other.
/// So the stream is opened when the first of them asks and closed when the last
/// stops asking, and "on" and "recording" are answered separately. Recording
/// with monitoring off is the useful combination on speakers, where hearing
/// yourself is not really on offer and feeding a live mic back into the room is
/// how you get a howl.
///
/// Session-lived on purpose — nothing is written to the config, so a launch
/// never starts with an open mic the player has forgotten about.
pub struct MicPassthrough {
    /// Found on the first switch-on and kept from then on, however many times
    /// it is toggled after that. See [`Devices`].
    devices: Option<Devices>,
    live: Option<Live>,
    /// Where captured samples go to be heard. Handed to the synth once at
    /// startup and shared for the rest of the session, so nothing downstream
    /// has to be rebuilt when passthrough is toggled — it simply stops feeding.
    monitor: Arc<Monitor>,
    /// Where captured samples go to be kept.
    tape: Arc<Tape>,
    /// Whether the player has asked to hear themselves. Kept apart from whether
    /// the device is open, which recording also has a say in.
    passthrough: bool,
    /// The rate samples reach the tape at, which is the synth's rate rather than
    /// the microphone's — they are resampled on the way in so the synth can add
    /// them frame for frame. Whatever a recording is written out at has to
    /// match, or it plays back at the wrong pitch.
    rate: u32,
    /// Why the last attempt to open it failed, so the menu can say so rather
    /// than looking like the click did nothing.
    error: Option<String>,
}

impl Default for MicPassthrough {
    fn default() -> Self {
        Self {
            devices: None,
            live: None,
            monitor: Arc::new(Monitor::new()),
            tape: Arc::new(Tape::new()),
            passthrough: false,
            rate: 48_000,
            error: None,
        }
    }
}

impl MicPassthrough {
    /// Whether the player has switched passthrough on. Not the same as whether
    /// the device is open: a recording holds it open too.
    pub fn is_on(&self) -> bool {
        self.passthrough
    }

    /// The shared queue the synth adds to its output. Handed over once, at
    /// startup, and valid whether or not passthrough is ever switched on.
    pub fn monitor(&self) -> Arc<Monitor> {
        Arc::clone(&self.monitor)
    }

    /// Set when opening the device failed, cleared by anything that succeeds.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The rate captured samples arrive at, and so the rate a take has to be
    /// written out at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// The peak the microphone is sending, 0 to 1, decaying between peaks.
    /// Zero while the device is closed.
    ///
    /// Read straight off the microphone, before the gain, because setting the
    /// dial is what it is for: a bar that moves when you speak says the mic is
    /// heard, and a bar jammed at the top says the dial is turned up past what
    /// the microphone can take and should come down. It is also the only
    /// feedback you get in the menu, where there is no synth stream to sing
    /// along with yet, and the only feedback you get while recording without
    /// monitoring.
    pub fn level(&self) -> f32 {
        self.live
            .as_ref()
            .map(|live| f32::from_bits(live.level.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }

    pub fn toggle(&mut self) {
        self.passthrough = !self.passthrough;
        self.monitor.set_monitoring(self.passthrough);
        self.sync_device();
    }

    /// Whether the microphone is open and feeding a take.
    pub fn is_recording(&self) -> bool {
        self.tape.is_armed() && self.live.is_some()
    }

    /// Start keeping what the microphone hears. Returns false if the device
    /// could not be opened, which the caller should say out loud — a take that
    /// silently has no voice in it is worse than one that says why.
    pub fn start_recording(&mut self) -> bool {
        self.tape.arm();
        self.sync_device();

        if self.live.is_none() {
            self.tape.disarm();
            return false;
        }

        true
    }

    /// Stop keeping it, and release the device unless passthrough still wants
    /// it. Anything captured but not yet collected is left in the tape for one
    /// last [`MicPassthrough::collect_recording`].
    pub fn stop_recording(&mut self) {
        self.tape.disarm();
        self.sync_device();
    }

    /// Move whatever the microphone has captured since last time onto the end
    /// of `out`. Called once a frame while recording, and once more on the way
    /// out to sweep up the tail.
    pub fn collect_recording(&self, out: &mut Vec<f32>) {
        self.tape.collect(out);
    }

    /// Whether the frame loop ever fell so far behind that the take has a gap
    /// in it.
    pub fn recording_dropped_samples(&self) -> bool {
        self.tape.dropped.load(Ordering::Relaxed)
    }

    /// Open or close the device to match what is currently being asked of it.
    /// Every path that changes either reason to want it ends here, so there is
    /// one place that decides and no way for the two to disagree.
    fn sync_device(&mut self) {
        let wanted = self.passthrough || self.tape.is_armed();

        match (wanted, self.live.is_some()) {
            (true, false) => self.open(),
            (false, true) => {
                // Dropped first: this joins the capture thread, so nothing is
                // still writing by the time the queues are emptied.
                self.live = None;
                self.monitor.live.reset();
                self.error = None;
            }
            _ => {}
        }
    }

    fn open(&mut self) {
        if self.devices.is_none() {
            match Devices::open() {
                Ok(devices) => self.devices = Some(devices),
                Err(err) => {
                    log::warn!("microphone unavailable: {err}");
                    self.error = Some(err);
                    return;
                }
            }
        }

        let devices = self.devices.as_ref().expect("just opened above");
        self.rate = devices.synth_rate();

        match open_capture(devices, Arc::clone(&self.monitor), Arc::clone(&self.tape)) {
            Ok(live) => {
                self.live = Some(live);
                self.error = None;
            }
            Err(err) => {
                log::warn!("microphone unavailable: {err}");
                self.error = Some(err);
            }
        }
    }
}

/// Open the microphone and start feeding the monitor and the tape.
fn open_capture(devices: &Devices, monitor: Arc<Monitor>, tape: Arc<Tape>) -> Result<Live, String> {
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
        Sinks { monitor, tape },
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

/// Everything one captured sample is handed to. Both take it after the gain and
/// the limiter, so a recording is exactly what was heard rather than a second
/// version of it processed differently — one signal path, one answer to "why
/// does the file not sound like the room".
struct Sinks {
    monitor: Arc<Monitor>,
    tape: Arc<Tape>,
}

impl Sinks {
    fn push(&self, sample: f32) {
        self.monitor.push_live(sample);
        self.tape.push(sample);
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    sinks: Sinks,
    level: Arc<AtomicU32>,
    capture_rate: u32,
    synth_rate: u32,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($t:ty) => {
            build_capture::<$t>(device, config, sinks, level, capture_rate, synth_rate)
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
    sinks: Sinks,
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

                resampler.feed(sample, |sample| sinks.push(sample));
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

/// Quietest level a meter should draw anything for. Below roughly this there is
/// nothing to hear anyway, so an empty bar is the honest reading.
const METER_FLOOR_DB: f32 = -60.0;

/// How much of a meter a peak fills, 0 to 1.
///
/// In decibels because that is how loudness is heard: a linear bar sits flat
/// across most of its travel and then leaps, which reads as a microphone that
/// is not working until suddenly it is.
///
/// Shared so the menu and the recording bar cannot disagree about what a given
/// level looks like — the same voice has to move both bars the same distance,
/// or one of them is lying about the same number.
pub fn level_fraction(peak: f32) -> f32 {
    if peak <= 0.0 {
        return 0.0;
    }

    let db = 20.0 * peak.log10();
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

/// A finished recording of the microphone: mono samples at [`VoiceTake::rate`].
///
/// Mono because that is what the microphone is — one voice, folded down on the
/// way in — and holding two copies of it would only make the file twice the
/// size. The samples are the ones that went to the speakers, gain and limiter
/// included.
pub struct VoiceTake {
    samples: Vec<f32>,
    rate: u32,
}

impl VoiceTake {
    pub fn new(samples: Vec<f32>, rate: u32) -> Self {
        Self {
            samples,
            rate: rate.max(1),
        }
    }

    /// The loudest sample in the take. Zero means the microphone was open and
    /// heard nothing, which is worth telling somebody about before they go
    /// looking for their voice in the file.
    pub fn peak(&self) -> f32 {
        self.samples
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    /// Write the take as a mono 16-bit PCM WAV.
    ///
    /// Sixteen-bit rather than the float the samples already are, because this
    /// file is for opening in something else — every editor, player and phone
    /// reads 16-bit PCM, and the take has already been through a limiter so the
    /// headroom float would buy is headroom nothing is using.
    pub fn write_wav(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, self.wav_bytes())
    }

    /// The file [`VoiceTake::write_wav`] writes, as bytes.
    pub(crate) fn wav_bytes(&self) -> Vec<u8> {
        const HEADER_LEN: usize = 44;
        const BYTES_PER_SAMPLE: u32 = 2;
        const CHANNELS: u16 = 1;

        let data_len = self.samples.len() as u32 * BYTES_PER_SAMPLE;
        let mut out = Vec::with_capacity(HEADER_LEN + data_len as usize);

        out.extend_from_slice(b"RIFF");
        // Everything after this field: the header past it, plus the samples.
        out.extend_from_slice(&(HEADER_LEN as u32 - 8 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");

        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // rest of this chunk
        out.extend_from_slice(&1u16.to_le_bytes()); // uncompressed PCM
        out.extend_from_slice(&CHANNELS.to_le_bytes());
        out.extend_from_slice(&self.rate.to_le_bytes());
        out.extend_from_slice(&(self.rate * BYTES_PER_SAMPLE * CHANNELS as u32).to_le_bytes());
        out.extend_from_slice(&(BYTES_PER_SAMPLE as u16 * CHANNELS).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());

        for sample in &self.samples {
            // Clamped before scaling: the limiter should have seen to this
            // already, but a sample that slipped past would wrap from loud to
            // loud-the-other-way, which is a crack rather than a clip.
            let scaled = sample.clamp(-1.0, 1.0) * i16::MAX as f32;
            out.extend_from_slice(&(scaled.round() as i16).to_le_bytes());
        }

        out
    }
}

/// Plays a finished take back through the synth, by feeding the monitor's
/// second source the way the microphone feeds its first.
///
/// There is no clock here and no need for one. The synth drains the queue at
/// exactly the rate it plays samples, so topping it back up to [`PREVIEW_FILL`]
/// once a frame delivers them at exactly the rate they should be heard,
/// however irregular the frames are. A slow frame leaves the queue lower and
/// the next one puts more in.
///
/// It shares a clock with nothing, though, which is why this drifts against the
/// MIDI beside it: that is advanced by frame time and this by the audio device.
/// Over a take of any normal length the two stay together well enough to listen
/// to, and seeking puts them back in step.
pub struct VoicePlayback {
    monitor: Arc<Monitor>,
    take: Arc<VoiceTake>,
    /// The next sample to hand over. Everything before it has been queued,
    /// which is not the same as having been heard — see [`VoicePlayback::pause`].
    cursor: usize,
    playing: bool,
}

/// How many samples to keep queued ahead of the synth.
const PREVIEW_FILL: usize = 2048;

impl VoicePlayback {
    pub fn new(monitor: Arc<Monitor>, take: Arc<VoiceTake>) -> Self {
        Self {
            monitor,
            take,
            cursor: 0,
            playing: false,
        }
    }

    /// Follow whatever the MIDI player is doing, then top the queue up. Called
    /// once a frame.
    pub fn update(&mut self, playing: bool) {
        if playing != self.playing {
            if playing {
                self.playing = true;
            } else {
                self.pause();
            }
        }

        if !self.playing {
            return;
        }

        while self.monitor.preview.ring.len() < PREVIEW_FILL {
            let Some(&sample) = self.take.samples.get(self.cursor) else {
                break;
            };
            self.monitor.preview.ring.push(sample);
            self.cursor += 1;
        }
    }

    /// Stop, and give back the ground that was queued but never played, so
    /// resuming picks up where the sound stopped rather than a queue's worth
    /// past it.
    fn pause(&mut self) {
        self.playing = false;
        self.cursor = self.cursor.saturating_sub(self.monitor.preview.reset());
    }

    /// Jump to a fraction of the way through, discarding whatever was queued.
    pub fn seek(&mut self, fraction: f32) {
        self.monitor.preview.reset();
        self.cursor = (fraction.clamp(0.0, 1.0) as f64 * self.take.samples.len() as f64) as usize;
    }
}

impl Drop for VoicePlayback {
    /// Leaving the preview must not leave a second of somebody's voice queued to
    /// play into whatever comes next.
    fn drop(&mut self) {
        self.monitor.preview.reset();
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

    /// A monitor with its live half switched on, which is what passthrough
    /// being on amounts to.
    fn monitoring() -> Monitor {
        let monitor = Monitor::new();
        monitor.set_monitoring(true);
        monitor
    }

    #[test]
    fn the_monitor_is_silent_until_it_has_a_head_start() {
        let monitor = monitoring();

        // Nothing captured yet: the synth must get exact zeros, or switching
        // passthrough on would tick.
        assert_eq!(monitor.next(), 0.0);

        // Still short of the head start, so still silent.
        for _ in 0..PREFILL - 1 {
            monitor.push_live(0.5);
        }
        assert_eq!(monitor.next(), 0.0);

        // One more and it starts, from the oldest sample.
        monitor.push_live(0.5);
        assert_eq!(monitor.next(), 0.5);
    }

    #[test]
    fn the_monitor_goes_quiet_again_when_it_runs_dry() {
        let monitor = monitoring();
        for _ in 0..PREFILL {
            monitor.push_live(0.25);
        }

        for _ in 0..PREFILL {
            assert_eq!(monitor.next(), 0.25);
        }

        // Drained. Silence rather than anything stale, and it waits for a fresh
        // head start rather than chasing the microphone sample by sample.
        assert_eq!(monitor.next(), 0.0);
        monitor.push_live(0.25);
        assert_eq!(monitor.next(), 0.0, "should not restart on a single sample");
    }

    #[test]
    fn switching_off_drops_what_was_still_queued() {
        let monitor = monitoring();
        for _ in 0..PREFILL {
            monitor.push_live(0.75);
        }
        assert_eq!(monitor.next(), 0.75);

        monitor.set_monitoring(false);

        // Nothing trickles out after the microphone is released.
        assert_eq!(monitor.next(), 0.0);
        assert_eq!(monitor.live.ring.len(), 0);
    }

    #[test]
    fn a_microphone_open_only_to_record_is_not_heard() {
        // Recording with passthrough off is the useful combination on speakers.
        // The samples must be kept and not played, so nothing goes into the
        // live queue to be heard a moment later or left there to go stale.
        let monitor = Monitor::new();
        for _ in 0..PREFILL * 2 {
            monitor.push_live(0.9);
        }

        assert_eq!(monitor.next(), 0.0);
        assert_eq!(monitor.live.ring.len(), 0);
    }

    #[test]
    fn the_tape_keeps_what_the_monitor_is_not_playing() {
        let sinks = Sinks {
            monitor: Arc::new(Monitor::new()),
            tape: Arc::new(Tape::new()),
        };

        // Disarmed, the tape ignores everything: an open microphone must not
        // quietly fill a buffer nobody is going to read.
        sinks.push(0.5);
        let mut out = Vec::new();
        sinks.tape.collect(&mut out);
        assert!(out.is_empty());

        sinks.tape.arm();
        for n in 0..1000 {
            sinks.push(n as f32);
        }

        sinks.tape.collect(&mut out);
        assert_eq!(out.len(), 1000);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[999], 999.0);
        assert!(!sinks.tape.dropped.load(Ordering::Relaxed));

        // Collecting is draining: the next sweep only sees what arrived since.
        out.clear();
        sinks.tape.collect(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn arming_the_tape_discards_the_last_take() {
        let tape = Tape::new();
        tape.arm();
        tape.push(1.0);

        tape.disarm();
        tape.arm();

        let mut out = Vec::new();
        tape.collect(&mut out);
        assert!(out.is_empty(), "a fresh take started with stale samples");
    }

    #[test]
    fn a_tape_left_uncollected_says_so_rather_than_hiding_the_gap() {
        let tape = Tape::new();
        tape.arm();
        for n in 0..TAPE_CAPACITY + 10 {
            tape.push(n as f32);
        }

        assert!(tape.dropped.load(Ordering::Relaxed));

        // What it kept is the start of the take, not the end: the recording is
        // short, rather than missing its beginning.
        let mut out = Vec::new();
        tape.collect(&mut out);
        assert_eq!(out.len(), TAPE_CAPACITY);
        assert_eq!(out[0], 0.0);
    }

    #[test]
    fn a_take_plays_back_at_the_rate_the_synth_drains_it() {
        let monitor = Arc::new(Monitor::new());
        let take = Arc::new(VoiceTake::new(
            (0..5000).map(|n| n as f32).collect(),
            48_000,
        ));
        let mut playback = VoicePlayback::new(Arc::clone(&monitor), take);

        // Paused, nothing is queued and nothing is heard.
        playback.update(false);
        assert_eq!(monitor.next(), 0.0);

        // Playing, the queue fills to its mark and no further — the frame loop
        // cannot run ahead of the synth however often it is called.
        playback.update(true);
        playback.update(true);
        assert_eq!(monitor.preview.ring.len(), PREVIEW_FILL);

        for expected in 0..100 {
            assert_eq!(monitor.next(), expected as f32);
        }

        // Draining leaves room, and the next frame tops it back up.
        playback.update(true);
        assert_eq!(monitor.preview.ring.len(), PREVIEW_FILL);
    }

    #[test]
    fn pausing_a_take_gives_back_what_was_queued_but_never_heard() {
        let monitor = Arc::new(Monitor::new());
        let take = Arc::new(VoiceTake::new(
            (0..5000).map(|n| n as f32).collect(),
            48_000,
        ));
        let mut playback = VoicePlayback::new(Arc::clone(&monitor), take);

        playback.update(true);
        for _ in 0..100 {
            monitor.next();
        }

        playback.update(false);
        assert_eq!(
            monitor.preview.ring.len(),
            0,
            "sound carried on after pause"
        );

        // Resuming continues from the hundred that were actually heard, not
        // from the couple of thousand that had been handed over.
        playback.update(true);
        assert_eq!(monitor.next(), 100.0);
    }

    #[test]
    fn a_take_ends_rather_than_looping_or_repeating_its_tail() {
        let monitor = Arc::new(Monitor::new());
        let take = Arc::new(VoiceTake::new(vec![0.5; PREFILL + 10], 48_000));
        let mut playback = VoicePlayback::new(Arc::clone(&monitor), take);

        playback.update(true);
        for _ in 0..PREFILL + 10 {
            assert_eq!(monitor.next(), 0.5);
        }

        playback.update(true);
        assert_eq!(monitor.next(), 0.0);
        assert_eq!(monitor.next(), 0.0);
    }

    #[test]
    fn dropping_a_preview_does_not_leave_a_voice_queued() {
        let monitor = Arc::new(Monitor::new());
        let take = Arc::new(VoiceTake::new(vec![0.5; 5000], 48_000));

        let mut playback = VoicePlayback::new(Arc::clone(&monitor), take);
        playback.update(true);
        assert!(monitor.preview.ring.len() > 0);

        drop(playback);

        assert_eq!(monitor.preview.ring.len(), 0);
        assert_eq!(monitor.next(), 0.0);
    }

    #[test]
    fn a_take_finds_its_loudest_moment() {
        assert_eq!(VoiceTake::new(vec![0.1, -0.8, 0.3], 48_000).peak(), 0.8);
        // Silence has to read as exactly zero: it is what tells a player their
        // microphone was open and heard nothing at all.
        assert_eq!(VoiceTake::new(vec![0.0; 100], 48_000).peak(), 0.0);
        assert_eq!(VoiceTake::new(Vec::new(), 48_000).peak(), 0.0);
    }

    #[test]
    fn a_take_writes_a_wav_a_player_will_open() {
        let take = VoiceTake::new(vec![0.0, 1.0, -1.0, 0.5], 44_100);
        let wav = take.wav_bytes();

        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");

        // Every length field has to agree with the file that arrived, or a
        // player reads past the end or stops short of it.
        assert_eq!(wav.len(), 44 + 4 * 2);
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 8);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 8);

        // Mono, 16-bit, at the rate the take was captured at — a wrong rate
        // here is a file that plays back at the wrong pitch.
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 44_100);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);

        let samples: Vec<i16> = wav[44..]
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes(pair.try_into().unwrap()))
            .collect();
        assert_eq!(samples, vec![0, i16::MAX, -i16::MAX, 16384]);
    }

    #[test]
    fn a_sample_past_full_scale_clips_rather_than_wrapping() {
        // The limiter should mean this never happens. If it does, the sound to
        // make is a loud one, not the crack of a waveform folding over.
        let wav = VoiceTake::new(vec![2.0, -2.0], 48_000).wav_bytes();
        let samples: Vec<i16> = wav[44..]
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes(pair.try_into().unwrap()))
            .collect();
        assert_eq!(samples, vec![i16::MAX, -i16::MAX]);
    }

    #[test]
    fn the_meter_reads_in_decibels() {
        assert_eq!(level_fraction(0.0), 0.0, "silence");
        assert_eq!(level_fraction(1.0), 1.0, "full scale");

        // Half the bar is half the way up in decibels, not in amplitude: -30
        // dBFS, which is an amplitude of about 0.032.
        assert!((level_fraction(0.0316) - 0.5).abs() < 0.01);

        // A microphone sending nothing but its own noise leaves it near empty,
        // and anything past full scale cannot push it further.
        assert!(level_fraction(0.0001) < 0.05);
        assert_eq!(level_fraction(4.0), 1.0);
    }

    #[test]
    fn ring_hands_samples_over_in_order() {
        let ring = Ring::new(RING_CAPACITY, MAX_FILL);
        assert_eq!(ring.pop(), None);

        for n in 0..100 {
            assert!(ring.push(n as f32));
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
        let ring = Ring::new(RING_CAPACITY, MAX_FILL);
        for n in 0..RING_CAPACITY * 2 {
            ring.push(n as f32);
        }

        assert_eq!(ring.len(), MAX_FILL);
        // What it kept is the oldest: playback is behind, and what it has yet
        // to play is what should come out next.
        assert_eq!(ring.pop(), Some(0.0));
    }

    #[test]
    fn ring_says_when_it_had_to_drop_one() {
        let ring = Ring::new(8, 4);
        for _ in 0..4 {
            assert!(ring.push(1.0));
        }
        assert!(
            !ring.push(1.0),
            "a full ring claimed to have taken a sample"
        );
    }

    #[test]
    fn ring_keeps_its_place_across_a_wrap() {
        let ring = Ring::new(RING_CAPACITY, MAX_FILL);
        // Well past the point where the counters fold back around the slots.
        for n in 0..RING_CAPACITY * 3 {
            ring.push(n as f32);
            assert_eq!(ring.pop(), Some(n as f32));
        }
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn clearing_a_ring_says_how_much_it_threw_away() {
        let ring = Ring::new(RING_CAPACITY, MAX_FILL);
        for n in 0..50 {
            ring.push(n as f32);
        }
        ring.pop();

        assert_eq!(ring.clear(), 49);
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.clear(), 0);
    }
}
