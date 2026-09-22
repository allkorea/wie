import type { WorkletSynthesizer } from "spessasynth_lib";
const assets = { bank: "GeneralUser.sf3", processor: "/spessasynth_processor.min.js" };
import {
  AudioProtocolError,
  checkAudioId,
  MAX_AUDIO_BATCH,
  MAX_AUDIO_BYTES,
  MAX_AUDIO_PLAYBACKS,
  MAX_AUDIO_SEQUENCES,
  ownEvents,
  validateEvents,
} from "./audio-protocol";
import type { SchedulerCommand, SchedulerOutput, TransportEvent } from "./audio-protocol";
export type { TransportEvent } from "./audio-protocol";

type Definition = { duration: number; events: TransportEvent[]; bytes: number; buffers: Map<number, AudioBuffer> };
type MidiPort = { offset: number; availableAt: number; busy: boolean };
type Playback = {
  id: number;
  token: number;
  sources: Set<AudioBufferSourceNode>;
  port?: MidiPort;
  lastTime: number;
  ended?: boolean;
};
type AudioState = {
  synth: WorkletSynthesizer | null;
  ctx: AudioContext;
  clockAudioTime: number;
  clockPerformanceTime: number;
  midiGain: GainNode;
  pcmGain: GainNode;
  ports: MidiPort[];
};

let midiVolume = 0.5;
let pcmVolume = 0.5;
let activePlayer: WeakRef<AudioPlayer> | undefined;
export function setMasterVolume(value: number): void {
  midiVolume = value;
  activePlayer?.deref()?.updateVolumes();
}
export function setPcmVolume(value: number): void {
  pcmVolume = value;
  activePlayer?.deref()?.updateVolumes();
}
export function resumeAudio(): void {
  activePlayer?.deref()?.setPaused(false);
}
export function suspendAudio(): void {
  activePlayer?.deref()?.setPaused(true);
}
export function vibrate(duration: number, intensity: number): void {
  if (duration > 0 && intensity > 0) globalThis.navigator?.vibrate?.(duration);
}

export class AudioPlayer {
  private worker?: Worker;
  private audio?: AudioState;
  private midiReady?: Promise<void>;
  private unavailable = false;
  private disposed = false;
  private paused = false;
  private transport: Promise<void> = Promise.resolve();
  private readonly abort = new AbortController();
  private readonly definitions = new Map<number, Definition>();
  private readonly playbacks = new Map<number, Playback>();
  private readonly legacy = new Map<number, number>();
  private nextLegacy = 0x100000000;
  private sequence = 0;
  private bytes = 0;
  private bufferBytes = 0;
  private error: string | null = null;
  private readonly onError?: (error: Error) => void;

  constructor(onError?: (error: Error) => void) {
    this.onError = onError;
    activePlayer = new WeakRef(this);
  }

  private failed(error: unknown): void {
    if (this.disposed) return;
    const cause = error instanceof Error ? error : new Error(String(error));
    this.error ??= cause.message;
    this.onError?.(cause);
  }

  public reportError(message: string): void {
    this.failed(new AudioProtocolError(message));
  }
  public takeError(): string | null {
    const error = this.error;
    this.error = null;
    return error;
  }

  private state(): AudioState | undefined {
    if (this.disposed || this.unavailable) return undefined;
    if (!this.audio) {
      try {
        const ctx = new AudioContext();
        const midiGain = ctx.createGain();
        midiGain.gain.value = midiVolume;
        midiGain.connect(ctx.destination);
        const pcmGain = ctx.createGain();
        pcmGain.gain.value = pcmVolume;
        pcmGain.connect(ctx.destination);
        this.audio = {
          ctx,
          synth: null,
          midiGain,
          pcmGain,
          ports: [],
          clockAudioTime: ctx.currentTime,
          clockPerformanceTime: performance.timeOrigin + performance.now(),
        };
      } catch (error) {
        this.unavailable = true;
        console.warn("Audio output is unavailable:", error);
      }
    }
    return this.audio;
  }

  private readyMidi(state: AudioState): Promise<void> {
    return (this.midiReady ??= (async () => {
      const signal = AbortSignal.any([this.abort.signal, AbortSignal.timeout(30_000)]);
      let aborted!: () => void;
      const cancelled = new Promise<never>((_, reject) => {
        aborted = () => reject(signal.reason);
        signal.addEventListener("abort", aborted, { once: true });
      });
      try {
        const [{ WorkletSynthesizer }, buffer] = await Promise.race([
          Promise.all([
            import("spessasynth_lib"),
            fetch(assets.bank, {
              cache: "force-cache",
              signal,
            }).then((response) => {
              if (!response.ok) throw new Error(`Sound bank: ${response.status}`);
              return response.arrayBuffer();
            }),
            state.ctx.audioWorklet.addModule(assets.processor),
          ]),
          cancelled,
        ]);
        if (this.disposed) return;
        const synth = new WorkletSynthesizer(state.ctx);
        state.synth = synth;
        await Promise.race([
          Promise.all([synth.soundBankManager.addSoundBank(buffer, "main"), synth.isReady]),
          cancelled,
        ]);
        if (this.disposed) return;
        synth.connect(state.midiGain);
      } finally {
        signal.removeEventListener("abort", aborted);
      }
    })().catch((error) => {
      state.synth?.destroy();
      state.synth = null;
      if (!this.disposed) console.warn("MIDI output is unavailable:", error);
    }));
  }

  private send(command: SchedulerCommand): void {
    this.worker?.postMessage(command);
  }

  private scheduler(): void {
    if (this.worker) return;
    this.worker = new Worker(new URL("./audio-worker.ts", import.meta.url), { type: "module" });
    this.worker.onerror = (event) => this.failed(new Error(event.message || "Audio scheduler failed"));
    this.worker.onmessageerror = () => this.failed(new AudioProtocolError("Invalid audio scheduler message"));
    this.worker.onmessage = ({ data }: MessageEvent<SchedulerOutput>) => {
      if (!this.audio || this.disposed) return;
      try {
        this.output(this.audio, data);
      } catch (error) {
        this.failed(error);
      }
    };
    for (const [id, definition] of this.definitions)
      this.send({
        type: "register",
        id,
        duration: definition.duration,
        times: definition.events.map((event) => event[0]),
      });
    if (this.paused) this.send({ type: "pause" });
  }

  public register(id: number, duration: number, events: TransportEvent[]): void {
    if (!this.disposed) this.adoptRegistration(id, duration, ownEvents(duration, events));
  }

  // CPU Worker registration transfers these exact owned views; no second snapshot is needed.
  public adoptRegistration(id: number, duration: number, events: TransportEvent[]): void {
    if (this.disposed) return;
    checkAudioId(id);
    const bytes = validateEvents(duration, events);
    if (
      this.definitions.has(id) ||
      this.definitions.size >= MAX_AUDIO_SEQUENCES ||
      this.bytes + bytes > MAX_AUDIO_BYTES
    )
      throw new AudioProtocolError("Audio registration limit or duplicate ID");
    this.definitions.set(id, { duration, events, bytes, buffers: new Map() });
    this.bytes += bytes;
    this.send({ type: "register", id, duration, times: events.map((event) => event[0]) });
  }

  public unregister(id: number): void {
    const definition = this.definitions.get(id);
    if (!definition) return;
    for (const [handle, playback] of this.playbacks) if (playback.id === id) this.stop(handle);
    this.definitions.delete(id);
    this.bytes -= definition.bytes;
    for (const buffer of definition.buffers.values()) this.bufferBytes -= buffer.length * buffer.numberOfChannels * 4;
    definition.buffers.clear();
    this.send({ type: "unregister", id });
  }

  public play(handle: number, duration: number, events: TransportEvent[], repeat: boolean): void {
    if (this.disposed) return;
    const previous = this.legacy.get(handle);
    if (previous !== undefined) this.unregister(previous);
    const id = this.nextLegacy++;
    this.register(id, duration, events);
    this.legacy.set(handle, id);
    this.playRegistered(handle, id, repeat);
  }

  public playRegistered(handle: number, id: number, repeat: boolean): void {
    if (this.disposed) return;
    checkAudioId(handle);
    const definition = this.definitions.get(id);
    if (!definition || typeof repeat !== "boolean") throw new AudioProtocolError("Unknown audio sequence");
    this.stop(handle);
    if (this.playbacks.size >= MAX_AUDIO_PLAYBACKS) throw new AudioProtocolError("Too many audio playbacks");
    const playback: Playback = { id, token: ++this.sequence, sources: new Set(), lastTime: 0 };
    this.playbacks.set(handle, playback);
    void Promise.resolve()
      .then(async () => {
        if (this.disposed || this.playbacks.get(handle) !== playback) return;
        const state = this.state();
        if (!state) {
          this.playbacks.delete(handle);
          return;
        }
        if (definition.events.some((event) => event[1] === "midi")) {
          await this.readyMidi(state);
          if (this.disposed || this.playbacks.get(handle) !== playback) return;
          if (state.synth) {
            let port = state.ports.find((candidate) => !candidate.busy);
            if (!port) {
              if (state.ports.length >= MAX_AUDIO_PLAYBACKS) throw new AudioProtocolError("Too many MIDI ports");
              const offset = state.ports.length * 16;
              if (offset > 0) for (let channel = 0; channel < 16; channel++) state.synth.addNewChannel();
              port = { offset, availableAt: 0, busy: false };
              state.ports.push(port);
            }
            port.busy = true;
            playback.port = port;
          }
        }
        if (!this.paused && state.ctx.state === "suspended") {
          await state.ctx.resume();
          if (this.paused) await state.ctx.suspend();
          state.clockAudioTime = state.ctx.currentTime;
          state.clockPerformanceTime = performance.timeOrigin + performance.now();
        } else if (this.paused && state.ctx.state === "running") await state.ctx.suspend();
        if (this.disposed || this.playbacks.get(handle) !== playback) return;
        this.scheduler();
        const notBefore =
          state.clockPerformanceTime +
          (Math.max(state.ctx.currentTime, playback.port?.availableAt ?? 0) - state.clockAudioTime) * 1000;
        this.send({ type: "play", handle, id, token: playback.token, repeat, notBefore });
      })
      .catch((error) => {
        if (this.playbacks.get(handle) === playback) this.stop(handle);
        this.failed(error);
      });
  }

  private cleanup(state: AudioState, playback: Playback, time: number, immediate: boolean): void {
    if (playback.port && state.synth) {
      for (const at of immediate && time > state.ctx.currentTime ? [state.ctx.currentTime, time] : [time]) {
        for (let channel = 0; channel < 16; channel++) {
          for (const control of [64, 120, 123])
            state.synth.sendMessage([0xb0 | channel, control, 0], playback.port.offset, { time: at });
        }
      }
    }
    if (immediate)
      for (const source of playback.sources) {
        source.onended = null;
        source.stop();
        source.disconnect();
        source.buffer = null;
      }
    if (immediate) playback.sources.clear();
  }

  private output(state: AudioState, output: SchedulerOutput): void {
    if (output.type === "cleanup") {
      const playback = this.playbacks.get(output.handle);
      if (!playback || playback.token !== output.token) return;
      if (!Number.isFinite(output.deadline)) throw new AudioProtocolError("Invalid cleanup time");
      const time = Math.max(
        state.ctx.currentTime,
        state.clockAudioTime + (output.deadline - state.clockPerformanceTime) / 1000,
      );
      this.cleanup(state, playback, time, output.immediate);
      if (output.ended) {
        if (playback.port) {
          playback.port.busy = false;
          playback.port.availableAt = Math.max(time, playback.lastTime) + 0.001;
        }
        playback.port = undefined;
        playback.ended = true;
        if (!playback.sources.size) this.playbacks.delete(output.handle);
      }
      return;
    }
    if (output.type !== "events" || !Array.isArray(output.events) || output.events.length > MAX_AUDIO_BATCH)
      throw new AudioProtocolError("Invalid audio batch");
    for (const scheduled of output.events) {
      const playback = this.playbacks.get(scheduled.handle);
      if (!playback || playback.token !== scheduled.token) continue;
      const definition = this.definitions.get(scheduled.id);
      const event = definition?.events[scheduled.index];
      if (
        !event ||
        !definition ||
        playback.id !== scheduled.id ||
        !Number.isSafeInteger(scheduled.index) ||
        !Number.isFinite(scheduled.deadline)
      )
        throw new AudioProtocolError("Invalid audio event reference");
      const time = Math.max(
        state.ctx.currentTime,
        state.clockAudioTime + (scheduled.deadline - state.clockPerformanceTime) / 1000,
      );
      playback.lastTime = Math.max(playback.lastTime, time);
      if (event[1] === "midi") {
        if (playback.port) state.synth?.sendMessage(event[2], playback.port.offset, { time });
        continue;
      }
      const [, , channels, samplingRate, samples] = event;
      if (!samples.length) continue;
      let buffer = definition.buffers.get(scheduled.index);
      if (!buffer) {
        const bytes = samples.length * 4;
        if (this.bufferBytes + bytes > MAX_AUDIO_BYTES) throw new AudioProtocolError("PCM cache exceeds memory limit");
        buffer = state.ctx.createBuffer(channels, samples.length / channels, samplingRate);
        for (let channel = 0; channel < channels; channel++) {
          const data = buffer.getChannelData(channel);
          for (let frame = 0; frame < data.length; frame++) data[frame] = samples[frame * channels + channel]! / 32768;
        }
        definition.buffers.set(scheduled.index, buffer);
        this.bufferBytes += bytes;
      }
      const source = state.ctx.createBufferSource();
      source.buffer = buffer;
      source.connect(state.pcmGain);
      playback.sources.add(source);
      source.onended = () => {
        playback.sources.delete(source);
        source.disconnect();
        source.buffer = null;
        if (playback.ended && !playback.sources.size && this.playbacks.get(scheduled.handle) === playback)
          this.playbacks.delete(scheduled.handle);
      };
      source.start(time);
    }
  }

  public stop(handle: number): void {
    const playback = this.playbacks.get(handle);
    if (!playback) return;
    this.playbacks.delete(handle);
    this.send({ type: "stop", handle, token: playback.token });
    if (this.audio) {
      const time = Math.max(this.audio.ctx.currentTime, playback.lastTime) + 0.001;
      this.cleanup(this.audio, playback, time, true);
      if (playback.port) {
        playback.port.busy = false;
        playback.port.availableAt = time;
      }
    }
  }

  public updateVolumes(): void {
    if (this.audio) {
      this.audio.midiGain.gain.value = midiVolume;
      this.audio.pcmGain.gain.value = pcmVolume;
    }
  }

  public setPaused(paused: boolean): void {
    const changed = this.paused !== paused;
    this.paused = paused;
    if (this.disposed) return;
    if (paused) this.send({ type: "pause" });
    if (!this.audio || (!paused && !changed && this.audio.ctx.state === "running")) return;
    this.transport = this.transport
      .then(async () => {
        const state = this.audio;
        if (!state || this.disposed || this.paused !== paused) return;
        if (paused) await state.ctx.suspend();
        else {
          await state.ctx.resume();
          if (this.disposed || this.paused) return;
          state.clockAudioTime = state.ctx.currentTime;
          state.clockPerformanceTime = performance.timeOrigin + performance.now();
          this.send({ type: "resume" });
        }
      })
      .catch((error) => this.failed(error));
  }

  public dispose(): void {
    if (this.disposed) return;
    for (const handle of this.playbacks.keys()) this.stop(handle);
    this.disposed = true;
    this.abort.abort();
    if (activePlayer?.deref() === this) activePlayer = undefined;
    this.worker?.terminate();
    this.worker = undefined;
    this.definitions.clear();
    this.legacy.clear();
    this.bytes = this.bufferBytes = 0;
    this.audio?.synth?.destroy();
    if (this.audio) this.audio.synth = null;
    void this.audio?.ctx.close().catch((error) => console.warn("Failed to close audio context:", error));
    this.audio = undefined;
  }
}
