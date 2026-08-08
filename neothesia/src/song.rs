use midi_file::MidiTrack;

use crate::context::Context;

/// What is to be done with one track.
///
/// `Human` marks the track as *the player's part*, which is what makes playing
/// one-handed possible: hand a track to `Auto` and the app performs it while you
/// keep the rest. How your own parts are then treated is the
/// [`PerformMode`]'s business — HUMAN waits for them, HERO silences them and
/// rolls on.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PlayerConfig {
    Mute,
    Auto,
    Human,
}

/// How the player's parts are performed. Which parts are theirs is a separate,
/// per-track question — see [`PlayerConfig`].
///
/// Remembered on [`crate::context::Context`] for the session and applied to
/// each song as it loads, so choosing a mode once holds across songs.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PerformMode {
    /// The song rolls on, but the player's parts stay *silent* — theirs to
    /// perform, Guitar-Hero style. Never stalls.
    Hero,
    /// The song plays itself in full; the player may jam over the top and is
    /// graded on whatever they play. Never stalls, and claims no parts.
    Auto,
    /// Classic play-along: the song stalls and waits for the player's parts.
    Human,
}

#[derive(Debug, Clone)]
pub struct TrackConfig {
    pub track_id: usize,
    pub player: PlayerConfig,
    pub visible: bool,
}

#[derive(Default, Debug, Clone)]
pub struct SongConfig {
    pub tracks: Box<[TrackConfig]>,
}

impl SongConfig {
    /// Track assignments for a song about to be performed in `mode`.
    fn for_mode(tracks: &[MidiTrack], mode: PerformMode) -> Self {
        let mut config = Self::new(tracks, false);
        config.apply_mode(tracks, mode);
        config
    }

    /// Which tracks are the player's, for `mode`, in place.
    ///
    /// Only fills in a default, and only when the song does not already say:
    /// AUTO claims nothing, HUMAN takes one part (the first melodic track —
    /// waiting for both hands is not a sensible starting point), and HERO takes
    /// the lot, which is what makes a song you have not touched play as a
    /// Guitar-Hero chart.
    ///
    /// A song that *does* already have parts assigned keeps them, so handing one
    /// hand to the app survives switching modes. Muted tracks stay muted:
    /// muting is about one instrument, not about who performs.
    pub fn apply_mode(&mut self, tracks: &[MidiTrack], mode: PerformMode) {
        if mode == PerformMode::Auto {
            for config in self.tracks.iter_mut() {
                if config.player == PlayerConfig::Human {
                    config.player = PlayerConfig::Auto;
                }
            }
            return;
        }

        if self.tracks.iter().any(|t| t.player == PlayerConfig::Human) {
            return;
        }

        let playable = |track: &MidiTrack| {
            let is_drums = track.has_drums && !track.has_other_than_drums;
            !is_drums && !track.notes.is_empty()
        };

        for (config, track) in self.tracks.iter_mut().zip(tracks) {
            if config.player == PlayerConfig::Mute || !playable(track) {
                continue;
            }

            config.player = PlayerConfig::Human;

            if mode == PerformMode::Human {
                break;
            }
        }
    }

    /// Rainbow-keys fork: when `default_human` is set, the first melodic
    /// track (has notes, not pure drums) defaults to Human so play-along
    /// scoring works out of the box; everything else accompanies on Auto.
    fn new(tracks: &[MidiTrack], default_human: bool) -> Self {
        let mut human_assigned = !default_human;
        let tracks: Vec<_> = tracks
            .iter()
            .map(|t| {
                let is_drums = t.has_drums && !t.has_other_than_drums;

                let player = if !human_assigned && !is_drums && !t.notes.is_empty() {
                    human_assigned = true;
                    PlayerConfig::Human
                } else {
                    PlayerConfig::Auto
                };

                TrackConfig {
                    track_id: t.track_id,
                    player,
                    visible: !is_drums,
                }
            })
            .collect();
        Self {
            tracks: tracks.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Song {
    pub file: midi_file::MidiFile,
    pub config: SongConfig,
}

impl Song {
    /// Load a song to be performed in `mode` — the mode the player last chose,
    /// so picking a new song does not silently put them back in another one.
    pub fn new(file: midi_file::MidiFile, mode: PerformMode) -> Self {
        let config = SongConfig::for_mode(&file.tracks, mode);
        Self { file, config }
    }

    /// Re-assign this song's tracks for `mode`.
    pub fn set_mode(&mut self, mode: PerformMode) {
        self.config.apply_mode(&self.file.tracks, mode);
    }

    /// All tracks on Auto — for playback that must not wait on user input
    /// (e.g. previewing a freeplay recording).
    pub fn new_all_auto(file: midi_file::MidiFile) -> Self {
        let config = SongConfig::new(&file.tracks, false);
        Self { file, config }
    }

    pub fn from_env(ctx: &Context) -> Option<Self> {
        let args: Vec<String> = std::env::args().collect();
        let midi_file = if args.len() > 1 {
            midi_file::MidiFile::new(&args[1]).ok()
        } else if let Some(last) = ctx.config.last_opened_song() {
            midi_file::MidiFile::new(last).ok()
        } else {
            None
        };

        Some(Self::new(midi_file?, ctx.perform_mode))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A melodic track with `id`, or a drums-only one.
    fn track(id: usize, drums: bool) -> MidiTrack {
        let note = midi_file::MidiNote {
            start: std::time::Duration::ZERO,
            end: std::time::Duration::from_secs(1),
            duration: std::time::Duration::from_secs(1),
            note: 60,
            velocity: 100,
            channel: if drums { 9 } else { 0 },
            track_id: id,
            track_color_id: id,
        };

        MidiTrack {
            notes: Arc::from(vec![note]),
            events: Arc::from(vec![]),
            track_id: id,
            track_color_id: id,
            programs: Arc::from(vec![]),
            has_drums: drums,
            has_other_than_drums: !drums,
        }
    }

    fn humans(config: &SongConfig) -> Vec<usize> {
        config
            .tracks
            .iter()
            .filter(|t| matches!(t.player, PlayerConfig::Human))
            .map(|t| t.track_id)
            .collect()
    }

    /// Play-along needs a Human track — the first melodic one, so a drums-only
    /// track at the top of the file is not handed to the player.
    #[test]
    fn a_song_loaded_for_human_gets_one_human_track() {
        let tracks = [track(0, true), track(1, false), track(2, false)];
        let config = SongConfig::for_mode(&tracks, PerformMode::Human);

        assert_eq!(humans(&config), vec![1]);
    }

    /// AUTO claims nothing: the app performs the whole song and the player
    /// merely jams over it.
    #[test]
    fn a_song_loaded_for_auto_claims_nothing() {
        let tracks = [track(0, false), track(1, false)];
        let config = SongConfig::for_mode(&tracks, PerformMode::Auto);

        assert!(humans(&config).is_empty());
    }

    /// HERO claims every melodic part, so a song nobody has configured plays as
    /// a Guitar-Hero chart rather than playing itself.
    #[test]
    fn a_song_loaded_for_hero_claims_every_melodic_part() {
        let tracks = [track(0, true), track(1, false), track(2, false)];
        let config = SongConfig::for_mode(&tracks, PerformMode::Hero);

        assert_eq!(humans(&config), vec![1, 2]);
    }

    /// Handing one hand to the app is the point of the per-track buttons, so it
    /// has to survive switching how the song is performed — the mode only fills
    /// in a default when the song has not been told.
    #[test]
    fn a_part_handed_to_the_app_survives_a_mode_switch() {
        let tracks = [track(0, false), track(1, false)];
        let mut config = SongConfig::for_mode(&tracks, PerformMode::Hero);
        assert_eq!(humans(&config), vec![0, 1]);

        // "You take the left hand, I'll play the right."
        config.tracks[0].player = PlayerConfig::Auto;

        for mode in [PerformMode::Human, PerformMode::Hero] {
            config.apply_mode(&tracks, mode);
            assert_eq!(humans(&config), vec![1], "{mode:?} kept the assignment");
        }
    }

    /// Muting is about one instrument, not about who performs, so a mode switch
    /// leaves it alone — and nothing ever hands the player a drums-only track.
    #[test]
    fn muted_and_drum_tracks_are_never_claimed() {
        let tracks = [track(0, true), track(1, false), track(2, false)];
        let mut config = SongConfig::for_mode(&tracks, PerformMode::Auto);
        config.tracks[1].player = PlayerConfig::Mute;

        config.apply_mode(&tracks, PerformMode::Hero);

        assert_eq!(humans(&config), vec![2]);
        assert_eq!(config.tracks[0].player, PlayerConfig::Auto);
        assert_eq!(config.tracks[1].player, PlayerConfig::Mute);
    }
}
