use alloc::{boxed::Box, collections::BTreeMap, collections::BTreeSet, sync::Arc, vec, vec::Vec};

use smaf_player::{SmafEvent, parse_smaf};

use crate::{AudioCommand, AudioEventData, AudioHandle, AudioSequence, AudioSequenceId, AudioSink, TimedAudioEvent};

#[derive(Debug)]
pub enum AudioError {
    InvalidHandle,
}

pub struct Audio {
    sink: Box<dyn AudioSink>,
    files: BTreeMap<AudioHandle, AudioSequenceId>,
    playing: BTreeSet<AudioHandle>,
    last_audio_handle: AudioHandle,
}

impl Audio {
    pub fn new(sink: Box<dyn AudioSink>) -> Self {
        Self {
            sink,
            files: BTreeMap::new(),
            playing: BTreeSet::new(),
            last_audio_handle: 0,
        }
    }

    pub fn shutdown(&mut self) {
        for handle in core::mem::take(&mut self.playing) {
            self.sink.send(AudioCommand::Stop { handle });
        }
        for id in core::mem::take(&mut self.files).into_values() {
            self.sink.send(AudioCommand::Unregister { id });
        }
    }

    pub fn load_smaf(&mut self, data: &[u8]) -> Result<AudioHandle, AudioError> {
        let audio_handle = self.last_audio_handle;
        self.last_audio_handle = self.last_audio_handle.checked_add(1).ok_or(AudioError::InvalidHandle)?;
        let id = AudioSequenceId(self.last_audio_handle);
        let sequence = Arc::new(convert_smaf_events(parse_smaf(data)));
        self.files.insert(audio_handle, id);
        self.sink.send(AudioCommand::Register { id, sequence });

        Ok(audio_handle)
    }

    pub fn play(&mut self, audio_handle: AudioHandle, repeat: bool) -> Result<(), AudioError> {
        let id = *self.files.get(&audio_handle).ok_or(AudioError::InvalidHandle)?;

        self.stop(audio_handle);
        self.playing.insert(audio_handle);
        self.sink.send(AudioCommand::Play {
            handle: audio_handle,
            id,
            repeat,
        });

        Ok(())
    }

    pub fn stop(&mut self, audio_handle: AudioHandle) {
        if self.playing.remove(&audio_handle) {
            self.sink.send(AudioCommand::Stop { handle: audio_handle });
        }
    }

    pub fn close(&mut self, audio_handle: AudioHandle) -> Result<(), AudioError> {
        self.stop(audio_handle);

        let id = self.files.remove(&audio_handle).ok_or(AudioError::InvalidHandle)?;
        self.sink.send(AudioCommand::Unregister { id });

        Ok(())
    }
}

fn convert_smaf_events(events: Vec<(usize, SmafEvent)>) -> AudioSequence {
    let duration = events.iter().map(|(time, _)| *time as u64).max().unwrap_or(0);
    let events = events
        .into_iter()
        .filter_map(|(time, event)| {
            let data = match event {
                SmafEvent::Wave {
                    channel,
                    sampling_rate,
                    data,
                } => AudioEventData::Wave {
                    channels: channel,
                    sampling_rate,
                    samples: data,
                },
                SmafEvent::MidiNoteOn { channel, note, velocity } => AudioEventData::Midi(vec![0x90 | channel, note, velocity]),
                SmafEvent::MidiNoteOff { channel, note, velocity } => AudioEventData::Midi(vec![0x80 | channel, note, velocity]),
                SmafEvent::MidiProgramChange { channel, program } => AudioEventData::Midi(vec![0xc0 | channel, program]),
                SmafEvent::MidiControlChange { channel, control, value } => AudioEventData::Midi(vec![0xb0 | channel, control, value]),
                SmafEvent::MidiPitchBend { channel, value } => {
                    AudioEventData::Midi(vec![0xe0 | channel, (value & 0x7f) as u8, ((value >> 7) & 0x7f) as u8])
                }
                SmafEvent::MidiSysEx(data) => AudioEventData::Midi(data),
                SmafEvent::End => return None,
            };

            Some(TimedAudioEvent { time: time as u64, data })
        })
        .collect();

    AudioSequence { duration, events }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
    use std::sync::Mutex;

    use smaf_player::SmafEvent;

    use super::{Audio, convert_smaf_events};
    use crate::{AudioCommand, AudioEventData, AudioSequence, AudioSink, TimedAudioEvent};

    struct RecordingSink(Arc<Mutex<Vec<AudioCommand>>>);

    impl AudioSink for RecordingSink {
        fn send(&self, command: AudioCommand) {
            self.0.lock().unwrap().push(command);
        }
    }

    #[test]
    fn converts_smaf_events_to_timed_transport() {
        let sequence = convert_smaf_events(vec![
            (5, SmafEvent::MidiProgramChange { channel: 2, program: 7 }),
            (10, SmafEvent::MidiPitchBend { channel: 3, value: 0x1234 }),
            (15, SmafEvent::End),
        ]);

        assert_eq!(
            sequence,
            AudioSequence {
                duration: 15,
                events: vec![
                    TimedAudioEvent {
                        time: 5,
                        data: AudioEventData::Midi(vec![0xc2, 7]),
                    },
                    TimedAudioEvent {
                        time: 10,
                        data: AudioEventData::Midi(vec![0xe3, 0x34, 0x24]),
                    },
                ],
            }
        );
    }

    #[test]
    fn replay_stops_previous_playback_before_starting_again() {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let mut audio = Audio::new(Box::new(RecordingSink(commands.clone())));
        let handle = audio.load_smaf(&[]).unwrap();

        audio.play(handle, false).unwrap();
        audio.play(handle, true).unwrap();

        let commands = commands.lock().unwrap();
        let AudioCommand::Register { id, .. } = &commands[0] else {
            panic!("expected one registration before playback");
        };
        assert_eq!(
            commands[1],
            AudioCommand::Play {
                handle,
                id: *id,
                repeat: false
            }
        );
        assert_eq!(commands[2], AudioCommand::Stop { handle });
        assert_eq!(
            commands[3],
            AudioCommand::Play {
                handle,
                id: *id,
                repeat: true
            }
        );
        assert_eq!(commands.len(), 4);
    }

    #[test]
    fn close_stops_playback_and_removes_the_handle() {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let mut audio = Audio::new(Box::new(RecordingSink(commands.clone())));
        let handle = audio.load_smaf(&[]).unwrap();

        audio.play(handle, false).unwrap();
        audio.close(handle).unwrap();

        assert_eq!(commands.lock().unwrap()[2], AudioCommand::Stop { handle });
        assert!(matches!(commands.lock().unwrap()[3], AudioCommand::Unregister { .. }));
        assert!(audio.play(handle, false).is_err());

        let next = audio.load_smaf(&[]).unwrap();
        audio.play(next, true).unwrap();
        audio.shutdown();
        assert_eq!(commands.lock().unwrap()[6], AudioCommand::Stop { handle: next });
        assert!(audio.play(next, false).is_err());
        audio.shutdown();
        assert_eq!(commands.lock().unwrap().len(), 8);
    }
}
