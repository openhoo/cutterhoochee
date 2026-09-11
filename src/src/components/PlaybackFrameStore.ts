import { useSyncExternalStore } from "react";

export class PlaybackFrameStore {
  private frame: number;
  private readonly listeners = new Set<() => void>();

  constructor(frame = 0) {
    this.frame = frame;
  }

  getSnapshot = (): number => this.frame;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  set = (frame: number): void => {
    if (!Number.isSafeInteger(frame) || frame === this.frame) return;
    this.frame = frame;
    for (const listener of this.listeners) listener();
  };
}

export function usePlaybackFrame(store: PlaybackFrameStore): number {
  return useSyncExternalStore(store.subscribe, store.getSnapshot, store.getSnapshot);
}
