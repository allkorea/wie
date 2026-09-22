export type TransportEvent =
  | [time: number, kind: "midi", data: Uint8Array]
  | [time: number, kind: "wave", channels: number, samplingRate: number, samples: Int16Array];

export type SchedulerCommand =
  | { type: "register"; id: number; duration: number; times: number[] }
  | { type: "unregister"; id: number }
  | { type: "play"; handle: number; id: number; token: number; repeat: boolean; notBefore: number }
  | { type: "stop"; handle: number; token: number }
  | { type: "pause" }
  | { type: "resume" };

export type ScheduledEvent = { handle: number; id: number; token: number; index: number; deadline: number };
export type SchedulerOutput =
  | { type: "events"; events: ScheduledEvent[] }
  | { type: "cleanup"; handle: number; token: number; deadline: number; immediate: boolean; ended: boolean };

export const MAX_AUDIO_BYTES = 64 * 1024 * 1024;
export const MAX_AUDIO_EVENTS = 100_000;
export const MAX_AUDIO_SEQUENCES = 1024;
export const MAX_AUDIO_PLAYBACKS = 32;
export const MAX_AUDIO_BATCH = 64;

export class AudioProtocolError extends Error {
  override name = "AudioProtocolError";
}

export function checkAudioId(id: number): void {
  if (!Number.isSafeInteger(id) || id < 0) throw new AudioProtocolError("Invalid audio ID");
}

export function validateEvents(duration: number, events: TransportEvent[]): number {
  if (!Number.isSafeInteger(duration) || duration < 0 || !Array.isArray(events) || events.length > MAX_AUDIO_EVENTS)
    throw new AudioProtocolError("Invalid audio sequence");
  let previous = 0;
  let bytes = events.length * 32;
  const seen = new Set<Uint8Array | Int16Array>();
  for (const event of events) {
    if (!Array.isArray(event) || !Number.isSafeInteger(event[0]) || event[0] < previous || event[0] > duration)
      throw new AudioProtocolError("Invalid audio event time");
    previous = event[0];
    let payload: Uint8Array | Int16Array;
    if (event[1] === "midi" && event.length === 3 && event[2] instanceof Uint8Array) payload = event[2];
    else if (event[1] === "wave" && event.length === 5 && event[4] instanceof Int16Array) {
      if (
        !Number.isInteger(event[2]) ||
        event[2] < 1 ||
        event[2] > 32 ||
        !Number.isInteger(event[3]) ||
        event[3] < 1 ||
        event[3] > 384000 ||
        event[4].length % event[2] !== 0
      )
        throw new AudioProtocolError("Invalid PCM format");
      payload = event[4];
    } else throw new AudioProtocolError("Invalid audio event");
    if (!seen.has(payload)) {
      seen.add(payload);
      bytes += payload.byteLength;
      if (bytes > MAX_AUDIO_BYTES) throw new AudioProtocolError("Audio sequence exceeds memory limit");
    }
  }
  return bytes;
}

/** Snapshots exact views once; callers retain their buffers after registration. */
export function ownEvents(duration: number, events: TransportEvent[]): TransportEvent[] {
  validateEvents(duration, events);
  const midi = new Map<Uint8Array, Uint8Array>();
  const pcm = new Map<Int16Array, Int16Array>();
  return events.map((event): TransportEvent => {
    if (event[1] === "midi") {
      let data = midi.get(event[2]);
      if (!data) {
        data = event[2].slice();
        midi.set(event[2], data);
      }
      return [event[0], "midi", data];
    }
    let data = pcm.get(event[4]);
    if (!data) {
      data = event[4].slice();
      pcm.set(event[4], data);
    }
    return [event[0], "wave", event[2], event[3], data];
  });
}

export function audioTransfers(events: TransportEvent[]): ArrayBuffer[] {
  const buffers = new Set<ArrayBuffer>();
  for (const event of events) {
    const data = event[1] === "midi" ? event[2] : event[4];
    if (!(data.buffer instanceof ArrayBuffer) || data.byteOffset !== 0 || data.byteLength !== data.buffer.byteLength)
      throw new AudioProtocolError("Audio transfer requires an owned buffer");
    buffers.add(data.buffer);
  }
  return [...buffers];
}
