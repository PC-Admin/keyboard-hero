use midi_file::midly::{MidiMessage, num::u4};

use crate::{
    output_manager::OutputConnection,
    song::{PlayerConfig, Song},
};
use neothesia_core::piano_layout;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

/// Who performs the song, cycled by the in-game toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformMode {
    /// The song rolls but its target notes stay *silent* — the player
    /// supplies them, Guitar-Hero style. Graded like Auto.
    Hero,
    /// The song plays itself audibly; the player may jam over the top and
    /// is graded on whatever they play. Never stalls.
    Auto,
    /// Classic play-along: the song stalls and waits for the player.
    Human,
}

pub struct MidiPlayer {
    playback: midi_file::PlaybackState,
    output: OutputConnection,
    song: Song,
    play_along: PlayAlong,
    separate_channels: bool,
    /// HERO mode latch: only meaningful while no track is Human.
    hero: bool,
}

impl MidiPlayer {
    pub fn new(
        output: OutputConnection,
        song: Song,
        user_keyboard_range: piano_layout::KeyboardRange,
        separate_channels: bool,
    ) -> Self {
        Self::new_with_lead_in(
            output,
            song,
            user_keyboard_range,
            separate_channels,
            Duration::from_secs(3),
        )
    }

    pub fn new_with_lead_in(
        output: OutputConnection,
        song: Song,
        user_keyboard_range: piano_layout::KeyboardRange,
        separate_channels: bool,
        lead_in: Duration,
    ) -> Self {
        let mut player = Self {
            playback: midi_file::PlaybackState::new(lead_in, song.file.tracks.clone()),
            output,
            play_along: PlayAlong::new(user_keyboard_range),
            song,
            separate_channels,
            hero: false,
        };
        // Let's reset programs,
        // for timestamp 0 most likely all programs will be 0, so this should clean any leftovers
        // from previous songs
        player.send_midi_programs_for_timestamp(&player.playback.time());
        player.update(Duration::ZERO);

        player
    }

    pub fn song(&self) -> &Song {
        &self.song
    }

    /// When playing: returns midi events
    ///
    /// When paused: returns None
    pub fn update(&mut self, delta: Duration) -> Vec<&midi_file::MidiEvent> {
        // No-wait jam mode (no Human track): the song never stalls, so
        // targets nobody played must expire as silent misses rather than
        // pile up. In wait mode they persist — the song is waiting on them.
        let jam_mode = !self.has_human_track();
        self.play_along.update(jam_mode);

        let events = self.playback.update(delta);

        events.iter().for_each(|event| {
            let config = &self.song.config.tracks[event.track_id];

            let channel = if self.separate_channels {
                event.track_color_id as u8
            } else {
                event.channel
            };
            match config.player {
                PlayerConfig::Auto => {
                    // Jam modes (AUTO and HERO): the song's playable notes
                    // become targets, so whatever the user plays on top is
                    // graded for real — matched notes fire the effects,
                    // unplayed ones expire as silent misses. Channel 9 is
                    // percussion (drum hits, not keys), and notes off the
                    // keyboard can't be played, so neither becomes a target.
                    let note_key = match event.message {
                        MidiMessage::NoteOn { key, .. } | MidiMessage::NoteOff { key, .. } => {
                            Some(key.as_int())
                        }
                        _ => None,
                    };
                    let is_target = jam_mode
                        && event.channel != 9
                        && note_key.is_some_and(|k| self.play_along.covers(k));

                    // HERO mode: target notes stay silent — that part is the
                    // player's to perform. Everything else still sounds.
                    if !(self.hero && is_target) {
                        self.output // TODO: Send to multiple outputs
                            .midi_event(u4::new(channel), event.message);
                    }

                    if is_target {
                        self.play_along
                            .midi_event(MidiEventSource::File, &event.message);
                    }
                }
                PlayerConfig::Human => {
                    self.play_along
                        .midi_event(MidiEventSource::File, &event.message);

                    // In Human mode note events from the file are targets for the player,
                    // not notes to be played by the synthesizer. Keep forwarding controller
                    // and other non-note events so the track still sounds as intended.
                    if should_forward_human_event(&event.message) {
                        self.output.midi_event(u4::new(channel), event.message);
                    }
                }
                PlayerConfig::Mute => {}
            }
        });

        events
    }

    fn clear(&mut self) {
        self.output.stop_all();
    }
}

impl Drop for MidiPlayer {
    fn drop(&mut self) {
        self.clear();
    }
}

impl MidiPlayer {
    pub fn pause_resume(&mut self) {
        if self.playback.is_paused() {
            self.resume();
        } else {
            self.pause();
        }
    }

    pub fn pause(&mut self) {
        self.clear();
        self.playback.pause();
    }

    pub fn resume(&mut self) {
        self.playback.resume();
        self.play_along.clear();
    }

    fn send_midi_programs_for_timestamp(&self, time: &Duration) {
        for (&channel, &p) in self.song.file.program_track.program_for_timestamp(time) {
            self.output.midi_event(
                u4::new(channel),
                midi_file::midly::MidiMessage::ProgramChange {
                    program: midi_file::midly::num::u7::new(p),
                },
            );
        }
    }

    pub fn set_time(&mut self, time: Duration) {
        self.playback.set_time(time);

        // Discard all of the events till that point
        let events = self.playback.update(Duration::ZERO);
        std::mem::drop(events);

        self.clear();
        self.send_midi_programs_for_timestamp(&time);
    }

    pub fn rewind(&mut self, delta: i64) {
        let mut time = self.playback.time();

        if delta < 0 {
            let delta = Duration::from_millis((-delta) as u64);
            time = time.saturating_sub(delta);
        } else {
            let delta = Duration::from_millis(delta as u64);
            time = time.saturating_add(delta);
        }

        self.set_time(time);
    }

    pub fn percentage_to_time(&self, p: f32) -> Duration {
        Duration::from_secs_f32((p * self.playback.length().as_secs_f32()).max(0.0))
    }

    pub fn time_to_percentage(&self, time: &Duration) -> f32 {
        time.as_secs_f32() / self.playback.length().as_secs_f32()
    }

    pub fn set_percentage_time(&mut self, p: f32) {
        self.set_time(self.percentage_to_time(p));
    }

    pub fn leed_in(&self) -> &Duration {
        self.playback.leed_in()
    }

    pub fn length(&self) -> Duration {
        self.playback.length()
    }

    pub fn percentage(&self) -> f32 {
        self.playback.percentage()
    }

    pub fn is_finished(&self) -> bool {
        self.playback.is_finished()
    }

    pub fn time(&self) -> Duration {
        self.playback.time()
    }

    pub fn time_without_lead_in(&self) -> f32 {
        self.playback.time().as_secs_f32() - self.playback.leed_in().as_secs_f32()
    }

    pub fn is_paused(&self) -> bool {
        self.playback.is_paused()
    }
}

impl MidiPlayer {
    pub fn play_along(&self) -> &PlayAlong {
        &self.play_along
    }

    /// Drain Guitar-Hero hit events (correct/wrong) since the last frame.
    pub fn take_hit_events(&mut self) -> Vec<HitEvent> {
        self.play_along.take_hit_events()
    }

    /// Run play-along housekeeping (wrong-press expiry) without advancing
    /// playback. Needed while wait-mode has the song stalled: [`Self::update`]
    /// is skipped then, but mashed wrong keys must still be judged promptly.
    pub fn tick_play_along(&mut self) {
        self.play_along.update(false);
    }

    pub fn mode(&self) -> PerformMode {
        if self.has_human_track() {
            PerformMode::Human
        } else if self.hero {
            PerformMode::Hero
        } else {
            PerformMode::Auto
        }
    }

    /// Switch who performs, mid-song. HUMAN re-assigns the first melodic
    /// non-drum track (the same one the song-setup default picks); the jam
    /// modes set every Human track back to Auto, with HERO also muting the
    /// target notes. Ringing notes are silenced and pending targets dropped,
    /// so a stalled song resumes on the spot instead of waiting for keys
    /// that are no longer anyone's job.
    pub fn set_mode(&mut self, mode: PerformMode) {
        if mode == self.mode() {
            return;
        }

        self.clear();
        self.play_along.clear();
        self.hero = mode == PerformMode::Hero;

        if mode == PerformMode::Human {
            let mut assigned = false;
            for (i, track) in self.song.file.tracks.iter().enumerate() {
                let is_drums = track.has_drums && !track.has_other_than_drums;
                if !assigned && !is_drums && !track.notes.is_empty() {
                    self.song.config.tracks[i].player = PlayerConfig::Human;
                    assigned = true;
                }
            }
        } else {
            for track in self.song.config.tracks.iter_mut() {
                if matches!(track.player, PlayerConfig::Human) {
                    track.player = PlayerConfig::Auto;
                }
            }
        }
    }

    /// True when at least one track is set to Human, i.e. play-along scoring
    /// is meaningful.
    pub fn has_human_track(&self) -> bool {
        self.song
            .config
            .tracks
            .iter()
            .any(|t| matches!(t.player, PlayerConfig::Human))
    }

    pub fn user_midi_event(&mut self, channel: u8, message: &MidiMessage) {
        self.output.midi_event(u4::new(channel), *message);

        // Judged in every mode: in wait mode against the notes the song is
        // stalled on, in jam mode against the notes the song is playing —
        // so a wrong key buzzes either way, and a matching one scores.
        self.play_along.midi_event(MidiEventSource::User, message);
    }
}

pub enum MidiEventSource {
    File,
    User,
}

fn should_forward_human_event(message: &MidiMessage) -> bool {
    !matches!(
        message,
        MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. }
    )
}

type NoteId = u8;

/// Result of a play-along key press, consumed by the visual effects system.
#[derive(Debug, Clone, Copy)]
pub enum HitKind {
    /// User played a required note correctly. `delta` is the gap between the
    /// file note and the user press — smaller is more accurate. `late` means
    /// the press came after the file note, i.e. the song was stalled in
    /// wait-mode for `delta` before this key finally went down.
    Good { delta: Duration, late: bool },
    /// User played a note that the song did not ask for (expired unmatched).
    Wrong,
    /// The song asked for a note and nobody played it (no-wait jam mode
    /// only — in wait mode the song stalls instead). Fails silently: combo
    /// resets, no buzzer, no text.
    Miss,
}

#[derive(Debug, Clone, Copy)]
pub struct HitEvent {
    pub note_id: NoteId,
    pub kind: HitKind,
    /// When the *song* asked for this note, which is what makes it a chord:
    /// every note of a chord shares this instant no matter how raggedly the
    /// user rolled it, so the combo can count the chord once. `None` for a
    /// wrong note — the song never asked for it.
    pub chord: Option<Instant>,
}

#[derive(Debug, Default)]
struct PlayerStats {
    /// User notes that expired, or were simply wrong
    wrong_notes: usize,
    /// List of deltas of notes played early
    played_early: Vec<Duration>,
    /// List of deltas of notes played late
    played_late: Vec<Duration>,
}

impl PlayerStats {
    #[allow(unused)]
    fn timing_acurracy(&self) -> f64 {
        let all = self.played_early.len() + self.played_late.len();
        let early_count = self.count_too_early();
        let late_count = self.count_too_late();
        (early_count + late_count) as f64 / all as f64
    }

    fn count_too_early(&self) -> usize {
        // 500 is the same as expire time, so this does not make much sense, but we can chooses
        // better threshold later down the line
        Self::count_with_threshold(&self.played_early, Duration::from_millis(500))
    }

    fn count_too_late(&self) -> usize {
        // 160 to forgive touching the bottom
        Self::count_with_threshold(&self.played_late, Duration::from_millis(160))
    }

    fn count_with_threshold(events: &[Duration], threshold: Duration) -> usize {
        events
            .iter()
            .filter(|delta| **delta > threshold)
            .fold(0, |n, _| n + 1)
    }
}

#[derive(Debug)]
struct NotePress {
    timestamp: Instant,
}

#[derive(Debug)]
pub struct PlayAlong {
    user_keyboard_range: piano_layout::KeyboardRange,

    /// Notes required to proggres further in the song
    required_notes: HashMap<NoteId, NotePress>,
    /// List of user key press events that happened in last 500ms,
    /// used for play along leeway logic
    user_pressed_recently: HashMap<NoteId, NotePress>,
    /// File notes that had NoteOn event, but no NoteOff yet
    in_proggres_file_notes: HashSet<NoteId>,

    /// Correct/wrong hit events accumulated since the last frame, drained by
    /// the visual effects system (Guitar-Hero-style sparks & combo).
    hit_events: Vec<HitEvent>,

    stats: PlayerStats,
}

impl PlayAlong {
    fn new(user_keyboard_range: piano_layout::KeyboardRange) -> Self {
        Self {
            user_keyboard_range,
            required_notes: Default::default(),
            user_pressed_recently: Default::default(),
            in_proggres_file_notes: Default::default(),
            hit_events: Vec::new(),
            stats: PlayerStats::default(),
        }
    }

    /// Drain the correct/wrong hit events collected since the last call.
    pub fn take_hit_events(&mut self) -> Vec<HitEvent> {
        std::mem::take(&mut self.hit_events)
    }

    /// Is this note on the user's keyboard, i.e. can it be a target?
    pub fn covers(&self, note_id: u8) -> bool {
        self.user_keyboard_range.contains(note_id)
    }

    fn update(&mut self, expire_required: bool) {
        // Instead of calling .elapsed() per item let's fetch `now` once, and subtract it ourselves
        let now = Instant::now();
        let threshold = Duration::from_millis(500);

        // Retain only the items that are within the threshold; anything that
        // expired unmatched was a wrong note.
        let mut expired: Vec<NoteId> = Vec::new();
        self.user_pressed_recently.retain(|note_id, item| {
            let keep = now.duration_since(item.timestamp) <= threshold;
            if !keep {
                expired.push(*note_id);
            }
            keep
        });

        self.stats.wrong_notes += expired.len();
        for note_id in expired {
            self.hit_events.push(HitEvent {
                note_id,
                kind: HitKind::Wrong,
                chord: None,
            });
        }

        // Jam mode: targets nobody played within the window are silent
        // misses. (Wait mode keeps them — the song is stalled on them.)
        if expire_required {
            let mut missed: Vec<NoteId> = Vec::new();
            self.required_notes.retain(|note_id, press| {
                let keep = now.duration_since(press.timestamp) <= threshold;
                if !keep {
                    missed.push(*note_id);
                }
                keep
            });

            for note_id in missed {
                self.hit_events.push(HitEvent {
                    note_id,
                    kind: HitKind::Miss,
                    chord: None,
                });
            }
        }
    }

    fn user_press_key(&mut self, note_id: u8, active: bool) {
        let timestamp = Instant::now();

        if active {
            // Check if note has already been played by a file
            if let Some(required_press) = self.required_notes.remove(&note_id) {
                let delta = timestamp.duration_since(required_press.timestamp);
                self.stats.played_late.push(delta);
                self.hit_events.push(HitEvent {
                    note_id,
                    kind: HitKind::Good { delta, late: true },
                    chord: Some(required_press.timestamp),
                });
            } else {
                // This note was not played by file yet, place it in recents
                let got_replaced = self
                    .user_pressed_recently
                    .insert(note_id, NotePress { timestamp })
                    .is_some();

                if got_replaced {
                    self.stats.wrong_notes += 1
                }
            }
        }
    }

    fn file_press_key(&mut self, note_id: u8, active: bool) {
        let timestamp = Instant::now();
        if active {
            // Check if note got pressed earlier 500ms (user_pressed_recently)
            if let Some(press) = self.user_pressed_recently.remove(&note_id) {
                let delta = timestamp.duration_since(press.timestamp);
                self.stats.played_early.push(delta);
                self.hit_events.push(HitEvent {
                    note_id,
                    kind: HitKind::Good { delta, late: false },
                    chord: Some(timestamp),
                });
            } else {
                // Player never pressed that note, let it reach required_notes

                // Ignore overlapping notes
                if self.in_proggres_file_notes.contains(&note_id) {
                    return;
                }

                self.required_notes.insert(note_id, NotePress { timestamp });
            }

            self.in_proggres_file_notes.insert(note_id);
        } else {
            self.in_proggres_file_notes.remove(&note_id);
        }
    }

    fn press_key(&mut self, src: MidiEventSource, note_id: u8, active: bool) {
        if !self.user_keyboard_range.contains(note_id) {
            return;
        }

        match src {
            MidiEventSource::User => self.user_press_key(note_id, active),
            MidiEventSource::File => self.file_press_key(note_id, active),
        }
    }

    pub fn midi_event(&mut self, source: MidiEventSource, message: &MidiMessage) {
        match message {
            MidiMessage::NoteOn { key, .. } => self.press_key(source, key.as_int(), true),
            MidiMessage::NoteOff { key, .. } => self.press_key(source, key.as_int(), false),
            _ => {}
        }
    }

    pub fn clear(&mut self) {
        self.required_notes.clear();
        self.user_pressed_recently.clear();
        self.in_proggres_file_notes.clear();
    }

    pub fn are_required_keys_pressed(&self) -> bool {
        self.required_notes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on(key: u8) -> MidiMessage {
        MidiMessage::NoteOn {
            key: key.into(),
            vel: 90.into(),
        }
    }

    /// Jam mode: a target the user matches while it is still live is a Good
    /// hit with a real timing delta — the machine no longer self-reports.
    #[test]
    fn jam_target_matched_by_user_scores_a_hit() {
        let mut pa = PlayAlong::new(piano_layout::KeyboardRange::standard_88_keys());

        pa.midi_event(MidiEventSource::File, &note_on(60));
        pa.midi_event(MidiEventSource::User, &note_on(60));

        let events = pa.take_hit_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].kind, HitKind::Good { late: true, .. }));
        assert!(events[0].chord.is_some());
    }

    /// Jam mode: a target nobody plays expires as a silent miss — but only
    /// when expiry is on (in wait mode the song is stalled on it instead).
    #[test]
    fn jam_target_left_alone_expires_as_miss() {
        let mut pa = PlayAlong::new(piano_layout::KeyboardRange::standard_88_keys());

        pa.midi_event(MidiEventSource::File, &note_on(60));

        pa.update(false);
        std::thread::sleep(Duration::from_millis(550));
        pa.update(false); // wait mode: target must survive
        assert!(pa.take_hit_events().is_empty());
        assert!(!pa.are_required_keys_pressed());

        pa.update(true); // jam mode: now it expires
        let events = pa.take_hit_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].note_id, 60);
        assert!(matches!(events[0].kind, HitKind::Miss));
        assert!(pa.are_required_keys_pressed());
    }
}
