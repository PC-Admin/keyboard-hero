//! Sound effects for play-along mode: the fail buzzer, the lightning crack,
//! and the results-screen crowd.
//!
//! Everything plays on its own audio stream, separate from the MIDI synth —
//! so it sounds the same no matter which MIDI output is selected, external
//! keyboards included.
//!
//! The fail and lightning sounds are embedded clips (assets/), so they are
//! there for everyone and need no disk read at the moment they fire. The crowd
//! reactions live in `~/Music/FX` and are matched to grade bands by filename
//! suffix: a file ending `_A` (before the extension) plays for any A-grade
//! result, `_B` for B grades, and so on through `_C`, `_D` and `_F`. Several
//! candidates for one band? First in alphabetical order wins, so renaming is
//! all it takes to pick a different reaction. Missing files just mean a
//! silent crowd for that band.

use std::{collections::HashMap, io::Cursor, path::PathBuf};

static FAIL_SOUND: &[u8] = include_bytes!("../../../../assets/fail.ogg");
/// Dragon Studio "lightning strike" (freesound id 386161), a one-second crack.
static LIGHTNING_SOUND: &[u8] = include_bytes!("../../../../assets/lightning.mp3");

const FAIL_VOLUME: f32 = 0.7;
const LIGHTNING_VOLUME: f32 = 0.85;
const CROWD_VOLUME: f32 = 0.9;
const BANDS: [char; 5] = ['A', 'B', 'C', 'D', 'F'];

/// Scan `~/Music/FX` for `*_<band>.<ext>` files, one winner per band.
fn scan_crowd_tracks() -> HashMap<char, PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return HashMap::new();
    };
    let dir = PathBuf::from(home).join("Music/FX");

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return HashMap::new();
    };

    let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    files.sort();

    let mut tracks = HashMap::new();
    for path in files {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        for band in BANDS {
            if stem.ends_with(&format!("_{band}")) {
                tracks.entry(band).or_insert_with(|| path.clone());
            }
        }
    }
    tracks
}

pub struct Sfx {
    /// `None` when the audio device could not be opened; every effect is a
    /// no-op then (playing the song still works — the synth has its own
    /// stream).
    stream: Option<rodio::MixerDeviceSink>,
    fail_playing: Option<rodio::Player>,
    lightning_playing: Option<rodio::Player>,
    crowd_playing: Option<rodio::Player>,
    crowd_tracks: HashMap<char, PathBuf>,
}

impl Sfx {
    pub fn new() -> Self {
        let stream = rodio::DeviceSinkBuilder::open_default_sink()
            .map_err(|err| log::warn!("sfx audio unavailable: {err}"))
            .ok();
        Self {
            stream,
            fail_playing: None,
            lightning_playing: None,
            crowd_playing: None,
            crowd_tracks: scan_crowd_tracks(),
        }
    }

    /// Play the fail sound, cutting off one already in flight.
    pub fn fail(&mut self) {
        let Some(stream) = &self.stream else {
            return;
        };

        // Dropping the previous player stops its clip.
        self.fail_playing.take();

        match rodio::Decoder::new(Cursor::new(FAIL_SOUND)) {
            Ok(source) => {
                let player = rodio::Player::connect_new(stream.mixer());
                player.set_volume(FAIL_VOLUME);
                player.append(source);
                self.fail_playing = Some(player);
            }
            Err(err) => log::warn!("failed to decode fail.ogg: {err}"),
        }
    }

    /// Crack of thunder for a lightning strike, cutting off one already in
    /// flight — two bolts can only land a chain apart, but a restart could
    /// otherwise leave the old one ringing.
    pub fn lightning(&mut self) {
        let Some(stream) = &self.stream else {
            return;
        };

        self.lightning_playing.take();

        match rodio::Decoder::new(Cursor::new(LIGHTNING_SOUND)) {
            Ok(source) => {
                let player = rodio::Player::connect_new(stream.mixer());
                player.set_volume(LIGHTNING_VOLUME);
                player.append(source);
                self.lightning_playing = Some(player);
            }
            Err(err) => log::warn!("failed to decode lightning.mp3: {err}"),
        }
    }

    /// The crowd reacts to a results-screen grade ("A++", "C-", "F", ...).
    pub fn crowd(&mut self, grade: &str) {
        let Some(stream) = &self.stream else {
            return;
        };
        let Some(band) = grade.chars().next() else {
            return;
        };
        let Some(path) = self.crowd_tracks.get(&band) else {
            log::info!("no crowd track for grade band {band} in ~/Music/FX");
            return;
        };

        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                log::warn!("failed to read {path:?}: {err}");
                return;
            }
        };

        self.crowd_playing.take();

        match rodio::Decoder::new(Cursor::new(bytes)) {
            Ok(source) => {
                log::info!("crowd reacts to {grade}: {path:?}");
                let player = rodio::Player::connect_new(stream.mixer());
                player.set_volume(CROWD_VOLUME);
                player.append(source);
                self.crowd_playing = Some(player);
            }
            Err(err) => log::warn!("failed to decode {path:?}: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded clips must decode with the features this crate builds
    /// with — the lightning is an mp3, and rodio only reads those with the
    /// `mp3` feature on. Failing here beats a silent strike.
    #[test]
    fn embedded_clips_decode() {
        rodio::Decoder::new(Cursor::new(FAIL_SOUND)).expect("fail.ogg");
        rodio::Decoder::new(Cursor::new(LIGHTNING_SOUND)).expect("lightning.mp3");
    }

    /// Every crowd track on this machine must actually decode, or the crowd
    /// will silently no-show at the results screen. (Trivially passes where
    /// ~/Music/FX doesn't exist.)
    #[test]
    fn crowd_tracks_decode() {
        for (band, path) in scan_crowd_tracks() {
            let bytes = std::fs::read(&path).unwrap();
            if let Err(err) = rodio::Decoder::new(Cursor::new(bytes)) {
                panic!("band {band}: {path:?} does not decode: {err}");
            }
        }
    }

    /// The band mapping is filename-driven: `_X` suffix on the stem, first
    /// alphabetical file wins a contested band.
    #[test]
    fn crowd_scan_maps_suffixes_to_bands() {
        let tracks = scan_crowd_tracks();
        // Only meaningful where ~/Music/FX exists (it does on Michael's box,
        // and the scan degrades to empty elsewhere).
        for (band, path) in &tracks {
            let stem = path.file_stem().unwrap().to_str().unwrap();
            assert!(
                stem.ends_with(&format!("_{band}")),
                "{path:?} mapped to band {band}"
            );
        }
    }
}
