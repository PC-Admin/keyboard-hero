use midi_file::MidiTrack;

use crate::context::Context;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PlayerConfig {
    Mute,
    Auto,
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
    pub fn new(file: midi_file::MidiFile) -> Self {
        let config = SongConfig::new(&file.tracks, true);
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

        Some(Self::new(midi_file?))
    }
}
