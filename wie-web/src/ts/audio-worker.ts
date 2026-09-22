import type { SchedulerCommand, SchedulerOutput, ScheduledEvent } from "./audio-protocol";
import { MAX_AUDIO_BATCH, MAX_AUDIO_EVENTS, MAX_AUDIO_PLAYBACKS, MAX_AUDIO_SEQUENCES } from "./audio-protocol";

type Definition = { duration: number; times: number[] };
type Playback = {
  id: number;
  token: number;
  definition: Definition;
  repeat: boolean;
  startedAt: number;
  nextEvent: number;
  cleanupAt?: number;
  lastScheduledAt: number;
};

const definitions = new Map<number, Definition>();
const playbacks = new Map<number, Playback>();
const LOOKAHEAD_MS = 50;
const MAX_TICK_EVENTS = 256;
let timer: ReturnType<typeof setTimeout> | undefined;
let pausedAt: number | undefined;
const scope = self as unknown as {
  onmessage: ((message: MessageEvent<SchedulerCommand>) => void) | null;
  postMessage(message: SchedulerOutput): void;
};

function cleanup(handle: number, playback: Playback, deadline: number, immediate: boolean, ended: boolean) {
  scope.postMessage({
    type: "cleanup",
    handle,
    token: playback.token,
    deadline: performance.timeOrigin + deadline,
    immediate,
    ended,
  });
}

scope.onmessage = ({ data }) => {
  switch (data.type) {
    case "register":
      if (
        !Number.isSafeInteger(data.id) ||
        data.id < 0 ||
        definitions.has(data.id) ||
        definitions.size >= MAX_AUDIO_SEQUENCES ||
        !Number.isSafeInteger(data.duration) ||
        data.duration < 0 ||
        !Array.isArray(data.times) ||
        data.times.length > MAX_AUDIO_EVENTS
      )
        throw new Error("Invalid audio registration");
      for (let i = 0; i < data.times.length; i++) {
        const time = data.times[i]!;
        if (!Number.isSafeInteger(time) || time < (data.times[i - 1] ?? 0) || time > data.duration)
          throw new Error("Invalid audio time");
      }
      definitions.set(data.id, { duration: data.duration, times: data.times });
      return;
    case "unregister":
      definitions.delete(data.id);
      for (const [handle, playback] of playbacks)
        if (playback.id === data.id) {
          cleanup(handle, playback, Math.max(pausedAt ?? performance.now(), playback.lastScheduledAt), true, true);
          playbacks.delete(handle);
        }
      break;
    case "pause":
      pausedAt ??= performance.now();
      clearTimeout(timer);
      timer = undefined;
      return;
    case "resume":
      if (pausedAt !== undefined) {
        const elapsed = performance.now() - pausedAt;
        for (const playback of playbacks.values()) {
          playback.startedAt += elapsed;
          playback.lastScheduledAt += elapsed;
          if (playback.cleanupAt !== undefined) playback.cleanupAt += elapsed;
        }
        pausedAt = undefined;
      }
      break;
    case "play": {
      const definition = definitions.get(data.id);
      if (
        !definition ||
        !Number.isSafeInteger(data.token) ||
        data.token < 0 ||
        !Number.isSafeInteger(data.handle) ||
        data.handle < 0 ||
        !Number.isFinite(data.notBefore) ||
        typeof data.repeat !== "boolean" ||
        (!playbacks.has(data.handle) && playbacks.size >= MAX_AUDIO_PLAYBACKS)
      )
        throw new Error("Invalid audio playback");
      const previous = playbacks.get(data.handle);
      if (previous)
        cleanup(data.handle, previous, Math.max(pausedAt ?? performance.now(), previous.lastScheduledAt), true, true);
      const startedAt = Math.max(pausedAt ?? performance.now(), data.notBefore - performance.timeOrigin);
      playbacks.set(data.handle, {
        id: data.id,
        token: data.token,
        definition,
        repeat: data.repeat,
        startedAt,
        nextEvent: 0,
        lastScheduledAt: startedAt,
      });
      break;
    }
    case "stop": {
      const playback = playbacks.get(data.handle);
      if (playback?.token === data.token) {
        cleanup(data.handle, playback, Math.max(pausedAt ?? performance.now(), playback.lastScheduledAt), true, true);
        playbacks.delete(data.handle);
      }
      break;
    }
  }
  schedule();
};

function schedule() {
  clearTimeout(timer);
  timer = undefined;
  if (pausedAt !== undefined) return;
  const now = performance.now();
  const horizon = now + LOOKAHEAD_MS;
  let processed = 0;
  let batch: ScheduledEvent[] = [];
  const flush = () => {
    if (batch.length) scope.postMessage({ type: "events", events: batch });
    batch = [];
  };
  for (const [handle, playback] of playbacks) {
    if (playback.cleanupAt !== undefined) {
      if (playback.cleanupAt <= now) {
        flush();
        cleanup(handle, playback, playback.cleanupAt, false, true);
        playbacks.delete(handle);
      }
      continue;
    }
    while (processed < MAX_TICK_EVENTS) {
      const event = playback.definition.times[playback.nextEvent];
      const deadline = playback.startedAt + (event ?? playback.definition.duration);
      if (deadline > horizon) break;
      processed++;
      playback.lastScheduledAt = Math.max(playback.lastScheduledAt, deadline);
      if (event !== undefined) {
        batch.push({
          handle,
          id: playback.id,
          token: playback.token,
          index: playback.nextEvent++,
          deadline: performance.timeOrigin + deadline,
        });
        if (batch.length === MAX_AUDIO_BATCH) flush();
      } else {
        flush();
        cleanup(handle, playback, deadline, false, false);
        if (!playback.repeat || playback.definition.duration === 0) {
          playback.cleanupAt = deadline;
          break;
        }
        playback.startedAt = Math.max(deadline, now);
        playback.nextEvent = 0;
      }
    }
  }
  flush();
  let delay = Infinity;
  for (const playback of playbacks.values()) {
    const deadline =
      playback.cleanupAt ??
      playback.startedAt +
        (playback.definition.times[playback.nextEvent] ?? playback.definition.duration) -
        LOOKAHEAD_MS;
    delay = Math.min(delay, deadline - now);
  }
  if (delay !== Infinity) timer = setTimeout(schedule, Math.max(0, delay));
}
