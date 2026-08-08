use midi_file::MidiTrack;

use crate::context::Context;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PlayerConfig {
    Mute,
    Auto,
    Human,
}

/// Who performs the song, as chosen by the in-game toggle.
///
/// This is the high-level statement of intent; [`PlayerConfig`] is its
/// per-track consequence, and what the rest of the engine actually reads.
/// HUMAN is expressed entirely in the track assignments, so it travels with a
/// song. HERO differs from AUTO only in muting the target notes — nothing in
/// the track config distinguishes them — so the choice is remembered on
/// [`crate::context::Context`] for the session and applied to each song as it
/// is loaded.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
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
    /// Track assignments for a song about to be performed in `mode`. Only
    /// HUMAN wants a Human track; the jam modes leave everything on Auto and
    /// are told apart by the player, not by the track config.
    fn for_mode(tracks: &[MidiTrack], mode: PerformMode) -> Self {
        Self::new(tracks, mode == PerformMode::Human)
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

    /// The jam modes have nobody waiting on the player, and it is the absence
    /// of a Human track that says so. Getting this wrong is what used to strand
    /// a HERO player in a song that stalled — or reported the wrong mode.
    #[test]
    fn a_song_loaded_for_a_jam_mode_has_no_human_track() {
        let tracks = [track(0, false), track(1, false)];

        for mode in [PerformMode::Hero, PerformMode::Auto] {
            let config = SongConfig::for_mode(&tracks, mode);
            assert!(humans(&config).is_empty(), "{mode:?} wants no Human track");
        }
    }
}
