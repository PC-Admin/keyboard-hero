//! Failure sound for wrong notes in play-along mode.
//!
//! Plays an embedded "dirr-nirrr" Ogg clip (assets/fail.ogg) on its own audio
//! stream, separate from the MIDI synth — so it sounds the same no matter
//! which MIDI output is selected, external keyboards included.

use std::io::Cursor;

static FAIL_SOUND: &[u8] = include_bytes!("../../../../assets/fail.ogg");

const VOLUME: f32 = 0.7;

pub struct FailBuzzer {
    /// `None` when the audio device could not be opened; buzzing is a no-op
    /// then (playing the song still works — the synth has its own stream).
    stream: Option<rodio::MixerDeviceSink>,
    playing: Option<rodio::Player>,
}

impl FailBuzzer {
    pub fn new() -> Self {
        let stream = rodio::DeviceSinkBuilder::open_default_sink()
            .map_err(|err| log::warn!("fail buzzer audio unavailable: {err}"))
            .ok();
        Self {
            stream,
            playing: None,
        }
    }

    /// Play the fail sound, cutting off one already in flight.
    pub fn trigger(&mut self) {
        let Some(stream) = &self.stream else {
            return;
        };

        // Dropping the previous player stops its clip.
        self.playing.take();

        match rodio::Decoder::new(Cursor::new(FAIL_SOUND)) {
            Ok(source) => {
                let player = rodio::Player::connect_new(stream.mixer());
                player.set_volume(VOLUME);
                player.append(source);
                self.playing = Some(player);
            }
            Err(err) => log::warn!("failed to decode fail.ogg: {err}"),
        }
    }
}
