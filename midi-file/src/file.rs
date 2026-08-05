use crate::{MidiTrack, program_track::ProgramTrack, tempo_track::TempoTrack};
use midly::{Format, MetaMessage, Smf, Timing, TrackEventKind};
use std::{fs, path::Path, sync::Arc};

/// The meter a file is written in. Only the signature in force at the start is
/// tracked — mid-piece changes are rare, and a bar grid that is right for the
/// opening beats a grid that assumes 4/4 everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSignature {
    pub numerator: u8,
    /// Always a power of two: 4 for 3/4, 8 for 6/8.
    pub denominator: u8,
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self {
            numerator: 4,
            denominator: 4,
        }
    }
}

impl TimeSignature {
    /// Length of one bar in quarter notes: 3 for 3/4, also 3 for 6/8.
    pub fn quarters_per_bar(self) -> f32 {
        if self.denominator == 0 {
            return 4.0;
        }
        self.numerator as f32 * 4.0 / self.denominator as f32
    }
}

#[derive(Debug, Clone)]
pub struct MidiFile {
    pub name: String,
    pub format: Format,
    pub tracks: Arc<[MidiTrack]>,
    pub program_track: ProgramTrack,
    pub tempo_track: TempoTrack,
    pub measures: Arc<[std::time::Duration]>,
    pub time_signature: TimeSignature,
}

impl MidiFile {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let name = path
            .as_ref()
            .file_name()
            .ok_or(String::from("File not found"))?
            .to_string_lossy()
            .to_string();

        let data = match fs::read(path) {
            Ok(buff) => buff,
            Err(_) => return Err(String::from("Could Not Open File")),
        };

        let smf = match Smf::parse(&data) {
            Ok(smf) => smf,
            Err(_) => return Err(String::from("Midi Parsing Error (midly lib)")),
        };

        Self::from_parsed_smf(name, &smf)
    }

    pub fn from_smf(name: impl Into<String>, smf: &Smf<'_>) -> Result<Self, String> {
        Self::from_parsed_smf(name.into(), smf)
    }

    fn from_parsed_smf(name: String, smf: &Smf<'_>) -> Result<Self, String> {
        let u_per_quarter_note: u16 = match smf.header.timing {
            Timing::Metrical(t) => t.as_int(),
            Timing::Timecode(_fps, _u) => {
                return Err(String::from("Midi With Timecode Timing, Not Supported!"));
            }
        };

        if smf.tracks.is_empty() {
            return Err(String::from("Midi File Has No Tracks"));
        }

        let tempo_track = TempoTrack::build(&smf.tracks, u_per_quarter_note);

        let mut track_color_id = 0;
        let tracks: Vec<MidiTrack> = smf
            .tracks
            .iter()
            .enumerate()
            .map(|(id, events)| {
                let track = MidiTrack::new(id, track_color_id, &tempo_track, events);

                if !track.notes.is_empty() {
                    track_color_id += 1;
                }

                track
            })
            .collect();

        let time_signature = find_time_signature(&smf.tracks);

        let measures = {
            let last_note_end = tracks
                .iter()
                .fold(std::time::Duration::ZERO, |last, track| {
                    if let Some(note) = track.notes.last() {
                        last.max(note.start + note.duration)
                    } else {
                        last
                    }
                });

            // A bar is however many quarter notes the meter says, not always
            // four: a 3/4 minuet gets bar lines every three beats.
            let pulses_per_bar = ((u_per_quarter_note as f32
                * time_signature.quarters_per_bar())
            .round() as u64)
                .max(1);

            let mut masures = Vec::new();
            let mut time = std::time::Duration::ZERO;
            let mut id = 0;
            while time <= last_note_end {
                time = tempo_track.pulses_to_duration(id * pulses_per_bar);
                masures.push(time);
                id += 1;
            }

            masures
        };

        let program_track = ProgramTrack::new(&tracks);

        Ok(Self {
            name,
            format: smf.header.format,
            tracks: tracks.into(),
            program_track,
            tempo_track,
            measures: measures.into(),
            time_signature,
        })
    }
}

/// The earliest time signature in the file, across all tracks.
fn find_time_signature(tracks: &[Vec<midly::TrackEvent>]) -> TimeSignature {
    let mut best: Option<(u64, TimeSignature)> = None;

    for events in tracks {
        let mut pulses: u64 = 0;
        for event in events {
            pulses += event.delta.as_int() as u64;
            if let TrackEventKind::Meta(MetaMessage::TimeSignature(num, den_pow, _, _)) = event.kind
            {
                let sig = TimeSignature {
                    numerator: num.max(1),
                    denominator: 1u8.checked_shl(den_pow as u32).unwrap_or(4),
                };
                if best.is_none_or(|(at, _)| pulses < at) {
                    best = Some((pulses, sig));
                }
                break;
            }
        }
    }

    best.map(|(_, sig)| sig).unwrap_or_default()
}
