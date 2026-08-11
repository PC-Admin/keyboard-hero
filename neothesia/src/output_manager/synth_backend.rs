use std::{error::Error, path::Path, rc::Rc, sync::Arc, sync::mpsc::Receiver};

use crate::output_manager::OutputDescriptor;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use midi_file::midly::{self, num::u4};

#[cfg(all(feature = "fluid-synth", not(feature = "oxi-synth")))]
const SAMPLES_SIZE: usize = 1410;

/// Frames per callback to ask the device for.
///
/// This synth is played live, so the buffer is the delay between pressing a key
/// and hearing it. Left to the device default that is whatever the audio server
/// negotiated for everything else — commonly 1024 frames, about 21ms at 48kHz,
/// which is enough to feel. 256 is around 5ms and reads as immediate, while
/// staying far enough from the edge to keep the callback fed.
///
/// Asked for per stream on purpose, rather than by forcing the server's global
/// quantum: this way it applies while the game is running and leaves the rest of
/// the machine alone.
const TARGET_BUFFER_FRAMES: u32 = 256;

pub struct SynthBackend {
    _host: cpal::Host,
    device: cpal::Device,

    stream_config: cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    gain: f32,
}

impl SynthBackend {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let host = cpal::default_host();

        let device = host
            .default_output_device()
            .ok_or("failed to find a default output device")?;

        let config = device.default_output_config()?;
        let sample_format = config.sample_format();

        // Clamped to what the device will actually accept; if the backend will
        // not say what that is, leave the default rather than guess at it.
        let buffer_size = match config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                cpal::BufferSize::Fixed(TARGET_BUFFER_FRAMES.clamp(*min, *max))
            }
            cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
        };

        let mut stream_config: cpal::StreamConfig = config.into();
        stream_config.buffer_size = buffer_size;
        log::info!(
            "synth output: {} Hz, buffer {:?}",
            stream_config.sample_rate,
            stream_config.buffer_size
        );

        Ok(Self {
            _host: host,
            device,

            stream_config,
            sample_format,
            gain: 0.2,
        })
    }

    fn run<T: cpal::SizedSample + cpal::FromSample<f32>>(
        &self,
        rx: Receiver<SynthEvent>,
        path: &Path,
        bus: Option<Arc<crate::microphone::AudioBus>>,
    ) -> cpal::Stream {
        #[cfg(all(feature = "fluid-synth", not(feature = "oxi-synth")))]
        let mut next_value = fluidsynth_adapter(self, rx, path);

        #[cfg(all(feature = "oxi-synth", not(feature = "fluid-synth")))]
        let mut next_value = oxisynth_adapter(self, rx, path, self.gain);

        let err_fn = |err| eprintln!("an error occurred on stream: {err}");

        let channels = self.stream_config.channels as usize;

        let stream = self
            .device
            .build_output_stream(
                self.stream_config,
                move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
                    for frame in output.chunks_mut(channels) {
                        let (l, r) = next_value();

                        // Singing along rides out on the synth's own stream
                        // rather than one of its own — see `crate::microphone`
                        // for why. Zero whenever passthrough is off, so this
                        // costs a load and an add and needs no flag of its own.
                        let mic = bus.as_ref().map(|bus| bus.next()).unwrap_or(0.0);

                        let l = l + mic;
                        let r = r + mic;

                        // And straight back to be recorded, if a take is
                        // running. Taken from here rather than from the
                        // microphone because this is the performance — piano
                        // and singing already summed, exactly as the speakers
                        // got it. Folded to one channel: the microphone is mono
                        // and goes to both sides, so the width being given up
                        // is the piano's alone, and it buys a file half the
                        // size and a recorder that never has to think about
                        // channel counts.
                        if let Some(bus) = bus.as_ref() {
                            bus.record((l + r) * 0.5);
                        }

                        let l = T::from_sample(l);
                        let r = T::from_sample(r);

                        let channels = [l, r];

                        for (id, sample) in frame.iter_mut().enumerate() {
                            *sample = channels[id % 2];
                        }
                    }
                },
                err_fn,
                None,
            )
            .unwrap();
        stream.play().unwrap();

        stream
    }

    pub fn new_output_connection(
        &mut self,
        path: &Path,
        bus: Option<Arc<crate::microphone::AudioBus>>,
    ) -> SynthOutputConnection {
        let (tx, rx) = std::sync::mpsc::channel::<SynthEvent>();
        let stream = match self.sample_format {
            cpal::SampleFormat::I8 => self.run::<i8>(rx, path, bus.clone()),
            cpal::SampleFormat::I16 => self.run::<i16>(rx, path, bus.clone()),
            cpal::SampleFormat::I32 => self.run::<i32>(rx, path, bus.clone()),
            cpal::SampleFormat::I64 => self.run::<i64>(rx, path, bus.clone()),

            cpal::SampleFormat::U8 => self.run::<u8>(rx, path, bus.clone()),
            cpal::SampleFormat::U16 => self.run::<u16>(rx, path, bus.clone()),
            cpal::SampleFormat::U32 => self.run::<u32>(rx, path, bus.clone()),
            cpal::SampleFormat::U64 => self.run::<u64>(rx, path, bus.clone()),

            cpal::SampleFormat::F32 => self.run::<f32>(rx, path, bus.clone()),
            cpal::SampleFormat::F64 => self.run::<f64>(rx, path, bus.clone()),
            sample_format => unimplemented!("Unsupported sample format '{sample_format}'"),
        };

        SynthOutputConnection {
            _stream: Rc::new(stream),
            tx,
        }
    }

    pub fn get_outputs(&self) -> Vec<OutputDescriptor> {
        vec![OutputDescriptor::Synth(None)]
    }
}

enum SynthEvent {
    SetGain(f32),
    Midi(oxisynth::MidiEvent),
}

#[derive(Clone)]
pub struct SynthOutputConnection {
    _stream: Rc<cpal::Stream>,
    tx: std::sync::mpsc::Sender<SynthEvent>,
}

impl SynthOutputConnection {
    pub fn midi_event(&self, channel: u4, msg: midly::MidiMessage) {
        let event = libmidi_to_oxisynth_event(channel, msg);
        self.tx.send(SynthEvent::Midi(event)).ok();
    }

    pub fn set_gain(&self, gain: f32) {
        self.tx.send(SynthEvent::SetGain(gain)).ok();
    }

    pub fn stop_all(&self) {
        for channel in 0..16 {
            self.tx
                .send(SynthEvent::Midi(oxisynth::MidiEvent::AllNotesOff {
                    channel,
                }))
                .ok();
            self.tx
                .send(SynthEvent::Midi(oxisynth::MidiEvent::AllSoundOff {
                    channel,
                }))
                .ok();
        }
    }
}

fn libmidi_to_oxisynth_event(channel: u4, message: midly::MidiMessage) -> oxisynth::MidiEvent {
    let channel = channel.as_int();
    match message {
        midly::MidiMessage::NoteOff { key, .. } => oxisynth::MidiEvent::NoteOff {
            channel,
            key: key.as_int(),
        },
        midly::MidiMessage::NoteOn { key, vel } => oxisynth::MidiEvent::NoteOn {
            channel,
            key: key.as_int(),
            vel: vel.as_int(),
        },
        midly::MidiMessage::Aftertouch { key, vel } => oxisynth::MidiEvent::PolyphonicKeyPressure {
            channel,
            key: key.as_int(),
            value: vel.as_int(),
        },
        midly::MidiMessage::Controller { controller, value } => {
            oxisynth::MidiEvent::ControlChange {
                channel,
                ctrl: controller.as_int(),
                value: value.as_int(),
            }
        }
        midly::MidiMessage::ProgramChange { program } => oxisynth::MidiEvent::ProgramChange {
            channel,
            program_id: program.as_int(),
        },
        midly::MidiMessage::ChannelAftertouch { vel } => oxisynth::MidiEvent::ChannelPressure {
            channel,
            value: vel.as_int(),
        },
        midly::MidiMessage::PitchBend { bend } => oxisynth::MidiEvent::PitchBend {
            channel,
            value: bend.0.as_int(),
        },
    }
}

#[cfg(all(feature = "oxi-synth", not(feature = "fluid-synth")))]
fn oxisynth_adapter<'a>(
    this: &SynthBackend,
    rx: Receiver<SynthEvent>,
    path: &Path,
    gain: f32,
) -> impl FnMut() -> (f32, f32) + 'a {
    let sample_rate = this.stream_config.sample_rate as f32;

    let mut synth = oxisynth::Synth::new(oxisynth::SynthDescriptor {
        sample_rate,
        gain,
        ..Default::default()
    })
    .unwrap();

    synth.set_reverb_params(&oxisynth::ReverbParams {
        roomsize: 0.5,
        damp: 0.3,
        width: 0.8,
        level: 0.7,
    });
    synth.set_chorus_params(&oxisynth::ChorusParams {
        nr: 4,
        level: 0.55,
        speed: 0.36,
        depth: 3.6,
        mode: Default::default(),
    });

    {
        let mut file = std::fs::File::open(path).unwrap();
        let font = oxisynth::SoundFont::load(&mut file).unwrap();
        synth.add_font(font, true);
    }

    move || {
        let (l, r) = synth.read_next();

        if let Ok(event) = rx.try_recv() {
            match event {
                SynthEvent::SetGain(gain) => {
                    synth.set_gain(gain);
                }
                SynthEvent::Midi(event) => {
                    synth.send_event(event).ok();
                }
            }
        }

        (l, r)
    }
}

#[cfg(all(feature = "fluid-synth", not(feature = "oxi-synth")))]
fn fluidsynth_adapter<'a>(
    this: &SynthBackend,
    rx: Receiver<SynthEvent>,
    path: &Path,
) -> impl FnMut() -> (f32, f32) + 'a {
    use fluidlite::{IsSettings, Settings};

    let synth = {
        let sample_rate = this.stream_config.sample_rate.0;

        let settings = Settings::new().unwrap();

        let rate = settings.pick::<_, f64>("synth.sample-rate").unwrap();
        rate.set(sample_rate as f64);

        let synth = fluidlite::Synth::new(settings).unwrap();
        synth.sfload(path, true).unwrap();

        synth
    };

    let mut sample_clock = 0;
    let mut buff: [f32; SAMPLES_SIZE] = [0.0f32; SAMPLES_SIZE];

    move || {
        let l = buff[sample_clock];
        let r = buff[sample_clock + 1];

        sample_clock += 2;

        if sample_clock == SAMPLES_SIZE {
            let buff: &mut [f32] = buff.as_mut();
            synth.write(buff).unwrap();
            sample_clock = 0;
        }

        if let Ok(e) = rx.try_recv() {
            match e {
                SynthEvent::SetGain(_g) => {
                    // TODO
                }
                SynthEvent::Midi(e) => match e {
                    oxisynth::MidiEvent::NoteOn { channel, key, vel } => {
                        synth.note_on(channel as u32, key as u32, vel as u32).ok();
                    }
                    oxisynth::MidiEvent::NoteOff { channel, key } => {
                        synth.note_off(channel as u32, key as u32).ok();
                    }
                    oxisynth::MidiEvent::PitchBend { channel, value } => {
                        synth.pitch_bend(channel as u32, value as u32).ok();
                    }
                    oxisynth::MidiEvent::ProgramChange {
                        channel,
                        program_id,
                    } => {
                        synth.program_change(channel as u32, program_id as u32).ok();
                    }
                    oxisynth::MidiEvent::ChannelPressure { channel, value } => {
                        synth.channel_pressure(channel as u32, value as u32).ok();
                    }
                    oxisynth::MidiEvent::PolyphonicKeyPressure {
                        channel,
                        key,
                        value,
                    } => {
                        synth
                            .key_pressure(channel as u32, key as u32, value as u32)
                            .ok();
                    }
                    oxisynth::MidiEvent::SystemReset => {
                        synth.system_reset().ok();
                    }
                    oxisynth::MidiEvent::ControlChange {
                        channel,
                        ctrl,
                        value,
                    } => {
                        synth.cc(channel as u32, ctrl as u32, value as u32).ok();
                    }
                    // TODO: Where are those for fluidsynth?
                    oxisynth::MidiEvent::AllNotesOff { .. } => {}
                    oxisynth::MidiEvent::AllSoundOff { .. } => {}
                },
            }
        }

        (l, r)
    }
}
