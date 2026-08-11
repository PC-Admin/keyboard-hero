use std::{
    collections::HashSet,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use midi_file::midly::{
    Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind,
};
use neothesia_core::render::{NoteLabels, WaterfallRenderer};

use crate::{
    context::Context,
    icons,
    microphone::{AudioTake, MicPassthrough, TakePlayback},
    scene::{
        freeplay::{FreeplayScene, on_async},
        playing_scene::{Keyboard, midi_player::MidiPlayer},
    },
    song::Song,
};

const TICKS_PER_BEAT: u16 = 480;
const TEMPO_MICROS_PER_BEAT: u32 = 500_000;
const TICKS_PER_SECOND: f64 = TICKS_PER_BEAT as f64 * 1_000_000.0 / TEMPO_MICROS_PER_BEAT as f64;

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum RecorderError {
    #[error("No note events recorded")]
    NoNotesFound,
    #[error("Nothing recorded")]
    NothingRecorded,
    #[error("Failed to write MIDI file")]
    Write,
    #[error("Failed to write WAV file")]
    WriteAudio,
    #[error("{0}")]
    MidiFileParse(String),
}

/// What ended up on a take.
///
/// Worth reporting in every case rather than only when it worked. The take is a
/// mix, so nothing in the file itself can tell a silent microphone from one that
/// never opened — both leave a recording of piano. Only the screen can say, and
/// on speakers, where you cannot really hear yourself, it is the only way to
/// find out before opening the file.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TakeOutcome {
    /// The playing and the singing, both on it.
    PianoAndVoice,
    /// The playing alone: the microphone was open and heard nothing, which
    /// usually means its dial is all the way down.
    SilentMicrophone,
    /// The playing alone: no microphone could be opened.
    NoMicrophone,
    /// Something was lost on the way, so the recording has a gap in it.
    Gapped,
    /// Nothing was recorded at all, because the output is not the built-in
    /// synth — there is no mix to tap when the notes are going to a MIDI device.
    NoAudio,
}

impl fmt::Display for TakeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PianoAndVoice => write!(f, "piano and voice"),
            Self::SilentMicrophone => write!(f, "piano only, microphone heard nothing"),
            Self::NoMicrophone => write!(f, "piano only, no microphone"),
            Self::Gapped => write!(f, "piano and voice, some samples dropped"),
            Self::NoAudio => write!(f, "MIDI only, no audio to record"),
        }
    }
}

#[derive(Default, Debug)]
pub enum RecorderStatus {
    #[default]
    Idle,
    RecordingFinished(Duration, TakeOutcome),
    Saved(String),
    Error(RecorderError),
}

impl fmt::Display for RecorderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => {}
            Self::RecordingFinished(duration, outcome) => {
                write!(f, "Recorded {:.1}s · {outcome}", duration.as_secs_f32())?;
            }
            Self::Error(err) => {
                write!(f, "{err}")?;
            }
            Self::Saved(what) => {
                write!(f, "Saved {what}")?;
            }
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct RecordedMidiEvent {
    timestamp: Duration,
    channel: u8,
    message: MidiMessage,
}

pub struct RecordingInProgressState {
    started_at: Instant,
    events: Vec<RecordedMidiEvent>,
    active_notes: HashSet<(u8, u8)>,
    /// The performance as it was heard — piano and singing already summed —
    /// swept out of the synth's output a frame at a time. Grown here rather
    /// than in the audio callback, which cannot allocate; see
    /// `microphone::MicPassthrough::collect_recording`.
    audio: Vec<f32>,
    /// False when the microphone would not open. Nothing in the take itself can
    /// say so, because a take with no singing on it is just a take of piano.
    microphone: bool,
}

impl RecordingInProgressState {
    fn finish_active_notes(&mut self, timestamp: Duration) {
        let mut active_notes: Vec<_> = self.active_notes.drain().collect();

        // TODO: What's the point of this sort?
        active_notes.sort_unstable();

        for (channel, key) in active_notes {
            self.events.push(RecordedMidiEvent {
                timestamp,
                channel,
                message: MidiMessage::NoteOff {
                    key: key.into(),
                    vel: 0.into(),
                },
            });
        }
    }
}

/// A finished take, in the two forms worth keeping: the notes, and the sound.
///
/// The audio is the performance as it was heard, piano and singing together,
/// which is the thing you would send somebody. The MIDI is the same playing as
/// data, which is the thing you would edit. Either may be missing and the take
/// is still worth keeping — singing over nothing is a perfectly good recording,
/// and so is playing with the microphone shut — so neither absence throws the
/// other away, which is what an earlier shape of this did to anyone who sang
/// without touching a key.
pub struct RecordedTake {
    duration: Duration,
    /// Missing when no notes were played. Only this half can drive a preview,
    /// because the preview is built around a `Song`.
    smf: Option<Smf<'static>>,
    audio: Option<Arc<AudioTake>>,
    outcome: TakeOutcome,
}

#[derive(Default)]
enum RecorderState {
    #[default]
    Idle,
    Recording(RecordingInProgressState),
    Recorded(RecordedTake),
}

#[derive(Default)]
pub struct FreeplayRecorder {
    state: RecorderState,
}

pub struct Preview {
    player: MidiPlayer,
    waterfall: WaterfallRenderer,
    note_labels: Option<NoteLabels>,
    /// The recording these notes came from, played back through the synth's own
    /// stream. When this is present it is the only thing making a sound: the
    /// piano is already on it, so the player above is muted and drives nothing
    /// but the waterfall and the keys.
    audio: Option<TakePlayback>,
}

impl Preview {
    fn new(keyboard: &Keyboard, song: Song, audio: Option<Arc<AudioTake>>, ctx: &Context) -> Self {
        let hidden_tracks: Vec<usize> = song
            .config
            .tracks
            .iter()
            .filter(|track| !track.visible)
            .map(|track| track.track_id)
            .collect();

        let mut waterfall = WaterfallRenderer::new(
            &ctx.gpu,
            &song.file.tracks,
            &hidden_tracks,
            &ctx.config,
            &ctx.transform,
            keyboard.layout().clone(),
        );

        let note_labels = ctx.config.note_labels().then_some(NoteLabels::new(
            *keyboard.pos(),
            waterfall.notes(),
            ctx.text_renderer_factory.new_renderer(),
        ));

        // With a recording of the mix in hand, the notes must not be played
        // again: the piano is already on it, and sounding the MIDI as well
        // would lay a second performance over the first, drifting apart as the
        // two clocks diverge. So the player drives the waterfall and the keys
        // and sends its notes nowhere, and everything you hear comes off the
        // recording. Without a recording there is nothing to hear otherwise,
        // and the synth plays as before.
        let output = match audio {
            Some(_) => crate::output_manager::OutputConnection::DummyOutput,
            None => ctx.output_manager.connection().clone(),
        };

        let mut player = MidiPlayer::new_with_lead_in(
            output,
            song,
            keyboard.layout().range.clone(),
            ctx.config.separate_channels(),
            Duration::ZERO,
            // Playing a recording back to the user, not asking them to perform
            // it — regardless of how they last played a song.
            crate::song::PerformMode::Auto,
        );
        player.pause();
        waterfall.update(player.time_without_lead_in() + ctx.config.animation_offset());

        Self {
            player,
            waterfall,
            note_labels,
            audio: audio.map(|take| TakePlayback::new(ctx.mic_passthrough.bus(), take)),
        }
    }

    pub fn resize(&mut self, keyboard: &Keyboard, ctx: &mut Context) {
        self.waterfall
            .resize(&ctx.config, keyboard.layout().clone());

        if let Some(note_labels) = self.note_labels.as_mut() {
            note_labels.set_pos(*keyboard.pos());
        }
    }

    pub fn update(&mut self, keyboard: &mut Keyboard, ctx: &mut Context, delta: Duration) {
        let midi_events = self.player.update(delta);
        keyboard.file_midi_events(&ctx.config, &midi_events);

        if self.player.is_finished() && !self.player.is_paused() {
            self.player.pause();
        }

        // Follows the notes rather than driving them: the recording starts,
        // stops and seeks with whatever the player is doing.
        if let Some(audio) = self.audio.as_mut() {
            audio.update(!self.player.is_paused());
        }

        let time = self.player.time_without_lead_in() + ctx.config.animation_offset();

        self.waterfall.update(time);

        if let Some(note_labels) = self.note_labels.as_mut() {
            note_labels.update(
                ctx.window_state.physical_size,
                ctx.window_state.scale_factor as f32,
                keyboard.renderer(),
                ctx.config.animation_speed(),
                time,
            );
        }
    }

    pub fn render<'pass>(&'pass mut self, rpass: &mut wgpu_jumpstart::RenderPass<'pass>) {
        self.waterfall.render(rpass);
        if let Some(note_labels) = self.note_labels.as_mut() {
            note_labels.render(rpass);
        }
    }
}

impl FreeplayRecorder {
    pub fn is_recording(&self) -> bool {
        matches!(self.state, RecorderState::Recording(_))
    }

    fn start(&mut self, mic: &mut MicPassthrough) {
        // Opens the microphone if the passthrough toggle has not already, and
        // leaves it open until this take ends. Recording does not need you to
        // be able to hear yourself, and on speakers that is just as well.
        self.begin(mic.start_recording());
    }

    /// The device-free half of [`FreeplayRecorder::start`], so the state
    /// machine can be exercised without opening a microphone.
    fn begin(&mut self, microphone: bool) {
        self.state = RecorderState::Recording(RecordingInProgressState {
            started_at: Instant::now(),
            events: Vec::new(),
            active_notes: HashSet::new(),
            audio: Vec::new(),
            microphone,
        });
    }

    /// Sweep the synth's buffer into the take. Called every frame while
    /// recording: the audio callback cannot grow a `Vec`, so somebody on this
    /// side has to, and falling behind is what costs samples.
    pub fn collect_audio(&mut self, mic: &MicPassthrough) {
        let RecorderState::Recording(in_progress) = &mut self.state else {
            return;
        };

        mic.collect_recording(&mut in_progress.audio);
    }

    /// Finish the take and keep whatever it has. `Err` means there is nothing
    /// to preview, not that the take was thrown away — the recording is kept
    /// either way and can still be saved.
    fn stop(&mut self, mic: &mut MicPassthrough) -> Result<(), RecorderError> {
        // Last sweep before the tape is disarmed, or the tail of the take is
        // left sitting in a buffer nobody will read again.
        self.collect_audio(mic);
        let rate = mic.rate();
        let gapped = mic.recording_dropped_samples();
        let mic_peak = mic.recording_mic_peak();
        mic.stop_recording();

        self.finish(rate, gapped, mic_peak)
    }

    /// Walk away from a take in progress, releasing the microphone. What was
    /// captured is not kept: leaving the screen mid-recording is not a request
    /// to preview or save anything.
    pub fn abandon(&mut self, mic: &mut MicPassthrough) {
        mic.stop_recording();
        self.state = RecorderState::Idle;
    }

    /// The device-free half of [`FreeplayRecorder::stop`]: turn what was
    /// gathered into a take, whatever it did or did not end up containing.
    fn finish(&mut self, rate: u32, gapped: bool, mic_peak: f32) -> Result<(), RecorderError> {
        let state = std::mem::take(&mut self.state);
        let RecorderState::Recording(mut in_progress) = state else {
            return Err(RecorderError::NothingRecorded);
        };

        let stop_time = in_progress.started_at.elapsed();
        in_progress.finish_active_notes(stop_time);

        let audio = (!in_progress.audio.is_empty())
            .then(|| Arc::new(AudioTake::new(std::mem::take(&mut in_progress.audio), rate)));

        // What the microphone did is a separate question from what was
        // recorded, now that the recording is a mix: a take full of piano is
        // what both a shut microphone and a silent one leave behind. The peak
        // is measured before the two are summed, and is the only thing that can
        // tell them apart. `HEARD` is a floor rather than zero because a live
        // input is never exactly silent — a mic left open in a quiet room still
        // sends its own noise, and calling that "voice" is how a take gets
        // reported as good when nobody sang.
        const HEARD: f32 = 0.01;
        let outcome = if audio.is_none() {
            TakeOutcome::NoAudio
        } else if !in_progress.microphone {
            TakeOutcome::NoMicrophone
        } else if mic_peak < HEARD {
            TakeOutcome::SilentMicrophone
        } else if gapped {
            TakeOutcome::Gapped
        } else {
            TakeOutcome::PianoAndVoice
        };

        let smf = to_smf(&in_progress.events).ok();
        let previewable = smf.is_some();

        // Says in one line what a take ended up with, because "why can I not
        // hear my singing" has several possible answers and they look identical
        // from the outside: no microphone, a silent one, no samples collected,
        // or a preview that was never built.
        log::info!(
            "recording: {:.1}s, {} mix samples at {rate} Hz, peak {:.3} ({:.1} dBFS), \
             rms {:.5} ({:.1} dBFS), mic peak {mic_peak:.3}, {outcome:?}, \
             {} notes to preview",
            stop_time.as_secs_f32(),
            audio.as_ref().map(|take| take.len()).unwrap_or(0),
            audio.as_ref().map(|take| take.peak()).unwrap_or(0.0),
            dbfs(audio.as_ref().map(|take| take.peak()).unwrap_or(0.0)),
            audio.as_ref().map(|take| take.rms()).unwrap_or(0.0),
            dbfs(audio.as_ref().map(|take| take.rms()).unwrap_or(0.0)),
            if previewable { "some" } else { "no" },
        );

        self.state = RecorderState::Recorded(RecordedTake {
            duration: stop_time,
            smf,
            audio,
            outcome,
        });

        previewable.then_some(()).ok_or(RecorderError::NoNotesFound)
    }

    fn duration(&self) -> Duration {
        match &self.state {
            RecorderState::Idle => Duration::ZERO,
            RecorderState::Recording(state) => state.started_at.elapsed(),
            RecorderState::Recorded(recorded_take) => recorded_take.duration,
        }
    }

    /// Whether the microphone is being captured into the take in progress.
    fn has_microphone(&self) -> bool {
        matches!(&self.state, RecorderState::Recording(state) if state.microphone)
    }

    pub fn push_event(&mut self, channel: u8, message: MidiMessage) {
        let RecorderState::Recording(in_progress) = &mut self.state else {
            return;
        };

        let timestamp = in_progress.started_at.elapsed();
        in_progress.events.push(RecordedMidiEvent {
            timestamp,
            channel,
            message,
        });

        match message {
            MidiMessage::NoteOn { key, .. } => {
                in_progress.active_notes.insert((channel, key.as_int()));
            }
            MidiMessage::NoteOff { key, .. } => {
                in_progress.active_notes.remove(&(channel, key.as_int()));
            }
            _ => {}
        }
    }

    fn recorded(&self) -> Option<&RecordedTake> {
        match &self.state {
            RecorderState::Recorded(take) => Some(take),
            _ => None,
        }
    }

    /// Whether there is anything a save would write.
    fn has_something_to_save(&self) -> bool {
        self.recorded()
            .is_some_and(|take| take.smf.is_some() || take.audio.is_some())
    }
}

/// An amplitude in decibels relative to full scale, which is the scale levels
/// are actually judged on — the difference between 0.4 and 0.04 reads as a
/// factor of ten and sounds like a fifth of the loudness.
fn dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        return -99.0;
    }
    20.0 * amplitude.log10()
}

fn duration_to_ticks(duration: Duration) -> u32 {
    (duration.as_secs_f64() * TICKS_PER_SECOND).round() as u32
}

pub fn update_preview_ui(scene: &mut FreeplayScene, ctx: &mut Context) {
    let top_bar_height = 30.0;

    let width = ctx.window_state.logical_size.width;

    let available = scene.preview.is_some();
    let savable = scene.recorder.has_something_to_save();
    let recording = scene.recorder.is_recording();

    let is_paused = scene
        .preview
        .as_ref()
        .map(|s| s.player.is_paused())
        .unwrap_or(true);

    // While recording, the meter beside the clock is the only sign the
    // microphone is being heard: on speakers your own voice covers the
    // passthrough almost completely, so watching the bar move is how you find
    // out before you play the take back rather than after.
    let status_label = if recording {
        let seconds = scene.recorder.duration().as_secs_f32();
        if scene.recorder.has_microphone() {
            format!("Recording {seconds:.1}s")
        } else {
            format!("Recording {seconds:.1}s · no microphone")
        }
    } else {
        scene.recorder_status.to_string()
    };

    let level = if recording {
        crate::microphone::level_fraction(ctx.mic_passthrough.level())
    } else {
        0.0
    };

    enum Msg {
        TogglePlay,
        Seek,
        GoBack,
        Record,
        Save,
        None,
    }

    let mut msg = Msg::None;

    nuon::translate().build(&mut scene.nuon, |ui| {
        nuon::quad()
            .size(width, top_bar_height)
            .color([37, 35, 42])
            .build(ui);

        nuon::translate().build(ui, |ui| {
            if nuon::button()
                .size(30.0, 30.0)
                .border_radius([5.0; 4])
                .icon(icons::left_arrow_icon())
                .build(ui)
            {
                msg = Msg::GoBack;
            }
            nuon::translate().x(30.0).add_to_current(ui);
        });

        nuon::label()
            .size(width, 30.0)
            .text(&status_label)
            .text_justify(nuon::TextJustify::Center)
            .build(ui);

        // Under the clock, spanning the middle of the bar, so it sits with the
        // thing it is reporting on rather than off in a corner.
        if recording {
            const METER_W: f32 = 120.0;
            const METER_H: f32 = 4.0;

            let x = (width - METER_W) / 2.0;
            nuon::quad()
                .pos(x, top_bar_height - METER_H - 3.0)
                .size(METER_W, METER_H)
                .color([24, 23, 28])
                .border_radius([2.0; 4])
                .build(ui);

            if level > 0.0 {
                nuon::quad()
                    .pos(x, top_bar_height - METER_H - 3.0)
                    .size(METER_W * level, METER_H)
                    .color(if level > 0.98 {
                        // Clipping before the app ever sees it: turn the
                        // microphone's own dial down.
                        [220, 110, 90]
                    } else {
                        [80, 200, 120]
                    })
                    .border_radius([2.0; 4])
                    .build(ui);
            }
        }

        nuon::translate().x(width).build(ui, |ui| {
            nuon::translate().x(-30.0).add_to_current(ui);

            if nuon::button()
                .size(30.0, 30.0)
                .border_radius([5.0; 4])
                .icon(if is_paused {
                    icons::play_icon()
                } else {
                    icons::pause_icon()
                })
                .font_color(if available {
                    [255, 255, 255, 255]
                } else {
                    [255, 255, 255, 100]
                })
                .build(ui)
                && available
            {
                msg = Msg::TogglePlay;
            }

            nuon::translate().x(-30.0).add_to_current(ui);

            if nuon::button()
                .size(30.0, 30.0)
                .border_radius([5.0; 4])
                .icon(icons::save_icon())
                // Lit by whether there is anything to write, which is not the
                // same as whether there is anything to play: a take that is all
                // singing and no notes has no preview and is still worth
                // keeping.
                .font_color(if savable {
                    [255, 255, 255, 255]
                } else {
                    [255, 255, 255, 100]
                })
                .build(ui)
                && savable
            {
                msg = Msg::Save;
            }

            nuon::translate().x(-30.0).add_to_current(ui);

            if nuon::button()
                .size(30.0, 30.0)
                .border_radius([5.0; 4])
                .icon(if scene.recorder.is_recording() {
                    icons::record_stop_icon()
                } else {
                    icons::record_icon()
                })
                .color(if scene.recorder.is_recording() {
                    [208, 18, 0, 255]
                } else {
                    [0, 0, 0, 0]
                })
                .hover_color(if scene.recorder.is_recording() {
                    [165, 47, 47]
                } else {
                    [97, 97, 97]
                })
                .preseed_color(if scene.recorder.is_recording() {
                    [145, 37, 37]
                } else {
                    [87, 87, 87]
                })
                .build(ui)
            {
                msg = Msg::Record;
            }
        });

        if let Some(state) = scene.preview.as_ref() {
            let length = state.player.length();
            let progress = state.player.percentage();
            let measures = &state.player.song().file.measures;

            nuon::translate().y(30.0).build(ui, |ui| {
                let event = nuon::click_area("FreeplayPreviewProgress")
                    .size(width, 45.0)
                    .build(ui);

                if event.is_pressed() {
                    msg = Msg::Seek;
                }

                nuon::quad().size(width, 45.0).color([37, 35, 42]).build(ui);
                nuon::quad()
                    .size(width * progress, 45.0)
                    .color([56, 145, 255])
                    .build(ui);

                if !length.is_zero() {
                    for measure in measures.iter() {
                        let x = (measure.as_secs_f32() / length.as_secs_f32()) * width;
                        nuon::quad()
                            .x(x)
                            .size(1.0, 45.0)
                            .color(if x < width * progress {
                                [255, 255, 255, 127]
                            } else {
                                [102, 102, 102, 255]
                            })
                            .build(ui);
                    }
                }
            });
        }

        // H-separator
        nuon::quad()
            .y(top_bar_height)
            .size(width, 1.0)
            .color([57, 55, 62])
            .build(ui);
    });

    match msg {
        Msg::TogglePlay => {
            toggle_preview_playback(scene);
        }
        Msg::Seek => {
            seek_preview_to_cursor(scene, ctx);
        }
        Msg::GoBack => {
            scene.leave(ctx);
        }
        Msg::Record => {
            handle_record_click(scene, ctx);
        }
        Msg::Save => {
            handle_save_click(scene, ctx);
        }
        Msg::None => {}
    }
}

fn handle_record_click(scene: &mut FreeplayScene, ctx: &mut Context) {
    if scene.recorder.is_recording() {
        let outcome = stop_recording(scene, ctx);
        scene.recorder_status = match outcome {
            Ok(voice) => RecorderStatus::RecordingFinished(scene.recorder.duration(), voice),
            Err(err) => RecorderStatus::Error(err),
        };
        return;
    }

    scene.keyboard.set_song_config(Default::default());
    scene.keyboard.reset_notes();

    // Dropped before the next take starts, so the previous take is not still
    // queued to play — and, more to the point, not still being recorded into
    // the new one.
    scene.preview = None;
    scene.recorder_status = RecorderStatus::default();
    scene.recorder.start(&mut ctx.mic_passthrough);
}

fn handle_save_click(scene: &mut FreeplayScene, ctx: &Context) {
    let Some(take) = scene.recorder.recorded() else {
        scene.recorder_status = RecorderStatus::Error(RecorderError::NothingRecorded);
        return;
    };

    let smf = take.smf.clone();
    let audio = take.audio.clone();

    // The dialog names whichever half exists; when both do, the notes are what
    // gets picked and the recording lands beside it under the same name. One
    // choice, two files, and a pair that stay together on disk.
    let mut dialog = rfd::AsyncFileDialog::new();
    dialog = if smf.is_some() {
        dialog
            .add_filter("midi", &["mid", "midi"])
            .set_file_name("freeplay-recording.mid")
    } else {
        dialog
            .add_filter("wav", &["wav"])
            .set_file_name("freeplay-recording.wav")
    };

    // TODO: `last_opened_song` is wrong in this context
    if let Some(path) = ctx.config.last_opened_song().and_then(|path| path.parent()) {
        dialog = dialog.set_directory(path);
    }

    scene
        .futures
        .push(on_async(dialog.save_file(), move |file, state, _ctx| {
            let Some(file) = file else {
                return;
            };

            let path = file.path().to_owned();
            let mut written = Vec::new();

            if let Some(smf) = smf {
                if smf.save(&path).is_err() {
                    state.recorder_status = RecorderStatus::Error(RecorderError::Write);
                    return;
                }
                written.push(file_name(&path));
            }

            if let Some(audio) = audio {
                let wav = path.with_extension("wav");
                if audio.write_wav(&wav).is_err() {
                    state.recorder_status = RecorderStatus::Error(RecorderError::WriteAudio);
                    return;
                }
                written.push(file_name(&wav));
            }

            state.recorder_status = RecorderStatus::Saved(format!(
                "{} to {}",
                written.join(" and "),
                path.parent().unwrap_or(&path).display()
            ));
        }));
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Finish the take, and build a preview of it if there are notes to build one
/// from. `Err` leaves the take intact and saveable — it only means there is
/// nothing to play back on screen.
fn stop_recording(
    scene: &mut FreeplayScene,
    ctx: &mut Context,
) -> Result<TakeOutcome, RecorderError> {
    scene.recorder.stop(&mut ctx.mic_passthrough)?;

    let Some(take) = scene.recorder.recorded() else {
        return Err(RecorderError::NothingRecorded);
    };
    let outcome = take.outcome;
    let audio = take.audio.clone();
    let smf = take.smf.clone().ok_or(RecorderError::NoNotesFound)?;

    let midi = midi_file::MidiFile::from_smf("freeplay-recording.mid", &smf)
        .map_err(RecorderError::MidiFileParse)?;
    // Preview must play itself back — never wait on user input.
    let song = Song::new_all_auto(midi);

    scene.keyboard.set_song_config(song.config.clone());
    scene.keyboard.reset_notes();

    scene.preview = Some(Preview::new(&scene.keyboard, song, audio, ctx));

    Ok(outcome)
}

fn seek_preview_to_cursor(scene: &mut FreeplayScene, ctx: &Context) {
    let Some(preview) = scene.preview.as_mut() else {
        return;
    };

    let width = ctx.window_state.logical_size.width.max(1.0);
    let percentage = (ctx.window_state.cursor_logical_position.x / width).clamp(0.0, 1.0);

    preview.player.set_percentage_time(percentage);
    // Sent the same fraction rather than the same timestamp: the take runs from
    // the first key to the last and the recording from the click of record to
    // the click of stop, so the two are the same length only by coincidence.
    if let Some(audio) = preview.audio.as_mut() {
        audio.seek(percentage);
    }
    scene.keyboard.reset_notes();
}

pub fn toggle_preview_playback(scene: &mut FreeplayScene) {
    let Some(preview) = scene.preview.as_mut() else {
        return;
    };

    preview.player.pause_resume();
}

fn to_smf(events: &[RecordedMidiEvent]) -> Result<Smf<'static>, RecorderError> {
    // Preview/export requires at least one played note, not just release/control data.
    let has_note_events = events
        .iter()
        .any(|event| matches!(event.message, MidiMessage::NoteOn { .. }));

    if !has_note_events {
        return Err(RecorderError::NoNotesFound);
    }

    let mut track = vec![
        TrackEvent {
            delta: 0.into(),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(TEMPO_MICROS_PER_BEAT.into())),
        },
        TrackEvent {
            delta: 0.into(),
            kind: TrackEventKind::Meta(MetaMessage::TimeSignature(4, 2, 24, 8)),
        },
    ];

    let mut previous_ticks = 0u32;
    for event in events {
        let current_ticks = duration_to_ticks(event.timestamp);
        let delta_ticks = current_ticks.saturating_sub(previous_ticks);
        previous_ticks = current_ticks;

        track.push(TrackEvent {
            delta: delta_ticks.into(),
            kind: TrackEventKind::Midi {
                channel: event.channel.into(),
                message: event.message,
            },
        });
    }

    track.push(TrackEvent {
        delta: 1.into(),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });

    Ok(Smf {
        header: Header {
            format: Format::SingleTrack,
            timing: Timing::Metrical(TICKS_PER_BEAT.into()),
        },
        tracks: vec![track],
    })
}

#[cfg(test)]
mod freeplay_recorder_tests {
    use super::*;

    const RATE: u32 = 48_000;
    /// A microphone peak that counts as somebody having sung.
    const MIC_HEARD: f32 = 0.4;
    /// One that does not: an open input in a quiet room, which is never exactly
    /// zero and must not be reported as a voice.
    const MIC_SILENT: f32 = 0.001;

    /// A recorder mid-take, with the microphone said to be open but nothing
    /// captured yet. `begin`/`finish` are used throughout rather than
    /// `start`/`stop` so none of this reaches for a real device.
    fn recording(microphone: bool) -> FreeplayRecorder {
        let mut recorder = FreeplayRecorder::default();
        recorder.begin(microphone);
        recorder
    }

    fn note_on(recorder: &mut FreeplayRecorder) {
        recorder.push_event(
            0,
            MidiMessage::NoteOn {
                key: 60.into(),
                vel: 100.into(),
            },
        );
    }

    /// Stand in for what the frame loop sweeps out of the synth's output.
    fn play(recorder: &mut FreeplayRecorder, samples: &[f32]) {
        let RecorderState::Recording(in_progress) = &mut recorder.state else {
            panic!("not recording");
        };
        in_progress.audio.extend_from_slice(samples);
    }

    #[test]
    fn restarting_recording_discards_previous_take_and_resets_event_count() {
        let mut recorder = recording(false);
        note_on(&mut recorder);

        assert!(recorder.finish(RATE, false, MIC_HEARD).is_ok());

        recorder.begin(false);

        assert!(recorder.is_recording());

        let error = recorder.finish(RATE, false, MIC_HEARD).expect_err("Empty");
        assert_eq!(error, RecorderError::NoNotesFound);
    }

    #[test]
    fn pedal_only_recording_is_rejected_for_preview_song() {
        let mut recorder = recording(false);

        recorder.push_event(
            0,
            MidiMessage::Controller {
                controller: 64.into(),
                value: 127.into(),
            },
        );
        recorder.push_event(
            0,
            MidiMessage::Controller {
                controller: 64.into(),
                value: 0.into(),
            },
        );

        let error = recorder
            .finish(RATE, false, MIC_HEARD)
            .expect_err("pedal-only recordings should not create preview songs");
        assert_eq!(error, RecorderError::NoNotesFound);
    }

    #[test]
    fn note_off_only_recording_is_rejected_for_preview_song() {
        let mut recorder = recording(false);

        recorder.push_event(
            0,
            MidiMessage::NoteOff {
                key: 60.into(),
                vel: 0.into(),
            },
        );

        let error = recorder
            .finish(RATE, false, MIC_HEARD)
            .expect_err("note-off-only recordings should not create preview songs");
        assert_eq!(error, RecorderError::NoNotesFound);
    }

    #[test]
    fn singing_over_no_notes_is_still_a_take_worth_keeping() {
        // No preview, because a preview is built around a song and there is no
        // song here. But the recording happened and must survive to be saved:
        // throwing it away for want of a keypress is how you lose a take.
        let mut recorder = recording(true);
        play(&mut recorder, &[0.4; 1000]);

        assert_eq!(
            recorder
                .finish(RATE, false, MIC_HEARD)
                .expect_err("no notes"),
            RecorderError::NoNotesFound
        );

        let take = recorder.recorded().expect("the take was thrown away");
        assert!(take.smf.is_none());
        assert!(take.audio.is_some());
        assert_eq!(take.outcome, TakeOutcome::PianoAndVoice);
        assert!(recorder.has_something_to_save());
    }

    #[test]
    fn playing_with_no_microphone_still_records_the_piano() {
        // The take is the synth's own output, so it has the playing on it
        // whether or not anything was sung over the top.
        let mut recorder = recording(false);
        note_on(&mut recorder);
        play(&mut recorder, &[0.4; 1000]);

        assert!(recorder.finish(RATE, false, 0.0).is_ok());

        let take = recorder.recorded().expect("the take was thrown away");
        assert!(take.smf.is_some());
        assert!(take.audio.is_some(), "the playing was not recorded");
        assert_eq!(take.outcome, TakeOutcome::NoMicrophone);
        assert!(recorder.has_something_to_save());
    }

    #[test]
    fn an_open_microphone_that_heard_nothing_says_so() {
        // Distinct from having no microphone at all, and the distinction is the
        // useful one: this is a dial turned all the way down, which the player
        // can fix, rather than a device that would not open. Neither can be read
        // off the take itself — both leave a recording of piano.
        let mut recorder = recording(true);
        note_on(&mut recorder);
        play(&mut recorder, &[0.4; 1000]);

        assert!(recorder.finish(RATE, false, MIC_SILENT).is_ok());
        assert_eq!(
            recorder.recorded().unwrap().outcome,
            TakeOutcome::SilentMicrophone,
            "a room's worth of noise was reported as singing"
        );
    }

    #[test]
    fn a_take_with_a_hole_in_it_is_not_passed_off_as_a_clean_one() {
        let mut recorder = recording(true);
        note_on(&mut recorder);
        play(&mut recorder, &[0.4; 1000]);

        assert!(recorder.finish(RATE, true, MIC_HEARD).is_ok());
        assert_eq!(recorder.recorded().unwrap().outcome, TakeOutcome::Gapped);
    }

    #[test]
    fn notes_going_somewhere_other_than_the_synth_leave_no_audio() {
        // A MIDI device sounds the notes itself, so there is no mix here to tap
        // and nothing to write to a WAV. The MIDI is still worth keeping.
        let mut recorder = recording(false);
        note_on(&mut recorder);

        assert!(recorder.finish(RATE, false, 0.0).is_ok());

        let take = recorder.recorded().unwrap();
        assert!(take.audio.is_none());
        assert_eq!(take.outcome, TakeOutcome::NoAudio);
        assert!(recorder.has_something_to_save());
    }

    #[test]
    fn nothing_at_all_leaves_nothing_to_save() {
        let mut recorder = recording(true);

        assert!(recorder.finish(RATE, false, 0.0).is_err());
        assert!(!recorder.has_something_to_save());
    }

    #[test]
    fn a_take_carries_the_rate_it_was_captured_at() {
        // The WAV header is written from this. Wrong here is a file that plays
        // back at the wrong pitch, which is the sort of thing nobody notices
        // until they open it somewhere else.
        let mut recorder = recording(true);
        play(&mut recorder, &[0.5; 44_100]);
        let _ = recorder.finish(44_100, false, MIC_HEARD);

        let voice = recorder.recorded().unwrap().audio.clone().unwrap();
        let wav = voice.wav_bytes();
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 44_100);
    }
}
