use alloc::string::String;

use js_sys::{Array, Int16Array, Uint8Array};
use wasm_bindgen::prelude::*;

use wie_backend::{AudioCommand, AudioEventData};

#[wasm_bindgen(module = "midi.ts")]
extern "C" {
    #[derive(Clone)]
    pub type AudioPlayer;

    #[wasm_bindgen(constructor)]
    pub fn new() -> AudioPlayer;

    #[wasm_bindgen(method)]
    pub fn dispose(this: &AudioPlayer);

    #[wasm_bindgen(method, catch)]
    fn register(this: &AudioPlayer, id: u32, duration: f64, events: Array) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch, js_name = playRegistered)]
    fn play_registered(this: &AudioPlayer, handle: u32, id: u32, repeat: bool) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn unregister(this: &AudioPlayer, id: u32) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn stop(this: &AudioPlayer, handle: u32) -> Result<(), JsValue>;

    #[wasm_bindgen(method, js_name = reportError)]
    fn report_error(this: &AudioPlayer, message: &str);

    #[wasm_bindgen(method, js_name = takeError)]
    pub fn take_error(this: &AudioPlayer) -> Option<String>;

    #[wasm_bindgen(js_name = setPcmVolume)]
    pub fn set_pcm_volume(value: f32);

    pub fn vibrate(duration: f64, intensity: u8);
}

pub struct AudioSink {
    player: AudioPlayer,
}

// The wasm frontend and its JavaScript audio bridge run on one thread.
unsafe impl Sync for AudioSink {}
unsafe impl Send for AudioSink {}

impl AudioSink {
    pub fn new(player: AudioPlayer) -> Self {
        Self { player }
    }
}

impl wie_backend::AudioSink for AudioSink {
    fn send(&self, command: AudioCommand) {
        let result = match command {
            AudioCommand::Register { id, sequence } => {
                let events = Array::new();
                for event in &sequence.events {
                    let value = Array::new();
                    value.push(&JsValue::from_f64(event.time as f64));

                    match &event.data {
                        AudioEventData::Midi(data) => {
                            value.push(&JsValue::from_str("midi"));
                            value.push(Uint8Array::from(data.as_slice()).as_ref());
                        }
                        AudioEventData::Wave {
                            channels,
                            sampling_rate,
                            samples,
                        } => {
                            value.push(&JsValue::from_str("wave"));
                            value.push(&JsValue::from(*channels));
                            value.push(&JsValue::from(*sampling_rate));
                            value.push(Int16Array::from(samples.as_slice()).as_ref());
                        }
                    }

                    events.push(value.as_ref());
                }

                self.player.register(id.0, sequence.duration as f64, events)
            }
            AudioCommand::Play { handle, id, repeat } => self.player.play_registered(handle, id.0, repeat),
            AudioCommand::Stop { handle } => self.player.stop(handle),
            AudioCommand::Unregister { id } => self.player.unregister(id.0),
        };
        if let Err(error) = result {
            self.player.report_error(&alloc::format!("Audio transport failed: {error:?}"));
        }
    }
}
