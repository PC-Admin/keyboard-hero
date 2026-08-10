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
    microphone::{MicPassthrough, VoicePlayback, VoiceTake},
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
    WriteVoice,
    #[error("{0}")]
    MidiFileParse(String),
}

/// What became of the singing over a take.
///
/// Worth reporting in every case rather than only when it worked: on speakers
/// you cannot really hear yourself, so "was my voice recorded" is a question
/// the screen has to answer or nobody finds out until they open the file.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VoiceOutcome {
    /// The microphone could not be opened, so the take is piano only.
    NoMicrophone,
    /// Captured, with something on it.
    Captured,
    /// Captured, and silent from end to end — the microphone was open and heard
    /// nothing, which usually means its dial is all the way down.
    Silent,
    /// Captured, but frames ran so slowly that samples were lost on the way.
    Gapped,
}

impl fmt::Display for VoiceOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMicrophone => write!(f, "no microphone"),
            Self::Captured => write!(f, "with voice"),
            Self::Silent => write!(f, "microphone heard nothing"),
            Self::Gapped => write!(f, "with voice, some samples dropped"),
        }
    }
}

#[derive(Default, Debug)]
pub enum RecorderStatus {
    #[default]
    Idle,
    RecordingFinished(Duration, VoiceOutcome),
    Saved(String),
    Error(RecorderError),
}

impl fmt::Display for RecorderStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => {}
            Self::RecordingFinished(duration, voice) => {
                write!(f, "Recorded {:.1}s · {voice}", duration.as_secs_f32())?;
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
    /// The singing, swept out of the microphone a frame at a time. Grown here
    /// rather than in the audio callback, which cannot allocate — see
    /// `microphone::MicPassthrough::collect_recording`.
    voice: Vec<f32>,
    /// False when the microphone would not open, so the take can say it is
    /// piano only rather than leaving somebody to wonder.
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

/// A finished take: the playing, the singing, or both.
///
/// Either half may be missing and the take is still worth keeping. Singing over
/// nothing is a perfectly good recording, and so is playing with the microphone
/// shut — so neither absence throws the other away, which is what an earlier
/// shape of this did to anyone who sang without touching a key.
pub struct RecordedTake {
    duration: Duration,
    /// Missing when no notes were played. Only the MIDI half can be previewed,
    /// because the preview is built around a `Song`.
    smf: Option<Smf<'static>>,
    voice: Option<Arc<VoiceTake>>,
    outcome: VoiceOutcome,
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
    /// The singing that went with these notes, played back through the synth's
    /// own stream so the two arrive together. `None` when the take has no voice.
    voice: Option<VoicePlayback>,
}

impl Preview {
    fn new(keyboard: &Keyboard, song: Song, voice: Option<Arc<VoiceTake>>, ctx: &Context) -> Self {
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

        let mut player = MidiPlayer::new_with_lead_in(
            ctx.output_manager.connection().clone(),
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
            voice: voice.map(|take| VoicePlayback::new(ctx.mic_passthrough.monitor(), take)),
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

        // Follows the notes rather than driving them: the voice starts, stops
        // and seeks with whatever the player is doing.
        if let Some(voice) = self.voice.as_mut() {
            voice.update(!self.player.is_paused());
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
            voice: Vec::new(),
            microphone,
        });
    }

    /// Sweep the microphone's buffer into the take. Called every frame while
    /// recording: the capture callback cannot grow a `Vec`, so somebody on this
    /// side has to, and falling behind is what costs samples.
    pub fn collect_voice(&mut self, mic: &MicPassthrough) {
        let RecorderState::Recording(in_progress) = &mut self.state else {
            return;
        };

        mic.collect_recording(&mut in_progress.voice);
    }

    /// Finish the take and keep whatever it has. `Err` means there is nothing
    /// to preview, not that the take was thrown away — the voice is kept
    /// either way and can still be saved.
    fn stop(&mut self, mic: &mut MicPassthrough) -> Result<(), RecorderError> {
        // Last sweep before the device closes, or the tail of the take is left
        // sitting in a buffer that is about to be dropped.
        self.collect_voice(mic);
        let rate = mic.rate();
        let gapped = mic.recording_dropped_samples();
        mic.stop_recording();

        self.finish(rate, gapped)
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
    fn finish(&mut self, rate: u32, gapped: bool) -> Result<(), RecorderError> {
        let state = std::mem::take(&mut self.state);
        let RecorderState::Recording(mut in_progress) = state else {
            return Err(RecorderError::NothingRecorded);
        };

        let stop_time = in_progress.started_at.elapsed();
        in_progress.finish_active_notes(stop_time);

        let voice = (!in_progress.voice.is_empty())
            .then(|| Arc::new(VoiceTake::new(std::mem::take(&mut in_progress.voice), rate)));

        let outcome = match (&voice, in_progress.microphone) {
            (_, false) => VoiceOutcome::NoMicrophone,
            (Some(take), _) if take.peak() > 0.0 => {
                if gapped {
                    VoiceOutcome::Gapped
                } else {
                    VoiceOutcome::Captured
                }
            }
            _ => VoiceOutcome::Silent,
        };

        let smf = to_smf(&in_progress.events).ok();
        let previewable = smf.is_some();

        self.state = RecorderState::Recorded(RecordedTake {
            duration: stop_time,
            smf,
            voice,
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
            .is_some_and(|take| take.smf.is_some() || take.voice.is_some())
    }
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

    // Dropped before the next take starts, so the previous take's voice is not
    // still queued to play while the new one is being sung.
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
    let voice = take.voice.clone();

    // The dialog names whichever half exists; when both do, the notes are what
    // gets picked and the voice lands beside it under the same name. One
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

            if let Some(voice) = voice {
                let wav = path.with_extension("wav");
                if voice.write_wav(&wav).is_err() {
                    state.recorder_status = RecorderStatus::Error(RecorderError::WriteVoice);
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
) -> Result<VoiceOutcome, RecorderError> {
    scene.recorder.stop(&mut ctx.mic_passthrough)?;

    let Some(take) = scene.recorder.recorded() else {
        return Err(RecorderError::NothingRecorded);
    };
    let outcome = take.outcome;
    let voice = take.voice.clone();
    let smf = take.smf.clone().ok_or(RecorderError::NoNotesFound)?;

    let midi = midi_file::MidiFile::from_smf("freeplay-recording.mid", &smf)
        .map_err(RecorderError::MidiFileParse)?;
    // Preview must play itself back — never wait on user input.
    let song = Song::new_all_auto(midi);

    scene.keyboard.set_song_config(song.config.clone());
    scene.keyboard.reset_notes();

    scene.preview = Some(Preview::new(&scene.keyboard, song, voice, ctx));

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
    // the first key to the last and the voice from the click of record to the
    // click of stop, so the two are the same length only by coincidence.
    if let Some(voice) = preview.voice.as_mut() {
        voice.seek(percentage);
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

    /// Stand in for what the frame loop sweeps out of the microphone.
    fn sing(recorder: &mut FreeplayRecorder, samples: &[f32]) {
        let RecorderState::Recording(in_progress) = &mut recorder.state else {
            panic!("not recording");
        };
        in_progress.voice.extend_from_slice(samples);
    }

    #[test]
    fn restarting_recording_discards_previous_take_and_resets_event_count() {
        let mut recorder = recording(false);
        note_on(&mut recorder);

        assert!(recorder.finish(RATE, false).is_ok());

        recorder.begin(false);

        assert!(recorder.is_recording());

        let error = recorder.finish(RATE, false).expect_err("Empty");
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
            .finish(RATE, false)
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
            .finish(RATE, false)
            .expect_err("note-off-only recordings should not create preview songs");
        assert_eq!(error, RecorderError::NoNotesFound);
    }

    #[test]
    fn singing_over_no_notes_is_still_a_take_worth_keeping() {
        // No preview, because a preview is built around a song and there is no
        // song here. But the singing happened and must survive to be saved:
        // throwing it away for want of a keypress is how you lose a take.
        let mut recorder = recording(true);
        sing(&mut recorder, &[0.4; 1000]);

        assert_eq!(
            recorder.finish(RATE, false).expect_err("no notes"),
            RecorderError::NoNotesFound
        );

        let take = recorder.recorded().expect("the take was thrown away");
        assert!(take.smf.is_none());
        assert!(take.voice.is_some());
        assert_eq!(take.outcome, VoiceOutcome::Captured);
        assert!(recorder.has_something_to_save());
    }

    #[test]
    fn playing_with_no_microphone_is_still_a_take_worth_keeping() {
        let mut recorder = recording(false);
        note_on(&mut recorder);

        assert!(recorder.finish(RATE, false).is_ok());

        let take = recorder.recorded().expect("the take was thrown away");
        assert!(take.smf.is_some());
        assert!(take.voice.is_none());
        assert_eq!(take.outcome, VoiceOutcome::NoMicrophone);
        assert!(recorder.has_something_to_save());
    }

    #[test]
    fn an_open_microphone_that_heard_nothing_says_so() {
        // Distinct from having no microphone at all, and the distinction is the
        // useful one: this is a dial turned all the way down, which the player
        // can fix, rather than a device that would not open.
        let mut recorder = recording(true);
        note_on(&mut recorder);
        sing(&mut recorder, &[0.0; 1000]);

        assert!(recorder.finish(RATE, false).is_ok());
        assert_eq!(
            recorder.recorded().unwrap().outcome,
            VoiceOutcome::Silent,
            "silence was reported as a good take"
        );
    }

    #[test]
    fn a_take_with_a_hole_in_it_is_not_passed_off_as_a_clean_one() {
        let mut recorder = recording(true);
        note_on(&mut recorder);
        sing(&mut recorder, &[0.4; 1000]);

        assert!(recorder.finish(RATE, true).is_ok());
        assert_eq!(recorder.recorded().unwrap().outcome, VoiceOutcome::Gapped);
    }

    #[test]
    fn nothing_at_all_leaves_nothing_to_save() {
        let mut recorder = recording(true);

        assert!(recorder.finish(RATE, false).is_err());
        assert!(!recorder.has_something_to_save());
    }

    #[test]
    fn a_take_carries_the_rate_it_was_captured_at() {
        // The WAV header is written from this. Wrong here is a file that plays
        // back at the wrong pitch, which is the sort of thing nobody notices
        // until they open it somewhere else.
        let mut recorder = recording(true);
        sing(&mut recorder, &[0.5; 44_100]);
        let _ = recorder.finish(44_100, false);

        let voice = recorder.recorded().unwrap().voice.clone().unwrap();
        let wav = voice.wav_bytes();
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 44_100);
    }
}
