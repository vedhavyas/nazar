//! The house's hero: the server decides which camera is big, and the home
//! follows. Nothing here orders cameras by its own idea of motion - the
//! hysteresis lives on the server, so every browser agrees.

import { api, type Camera } from './api';

/** Where the hero decision and its events come from. */
export interface HeroSource {
  /** The server's current hero, or null when nothing has moved. */
  hero(): Promise<string | null>;
  /** Follow the house's events, re-asking on each. Returns a teardown. */
  watch(onEvent: () => void): () => void;
}

/** A camera as the home renders it: one big, the rest small. */
export interface Tile {
  camera: Camera;
  hero: boolean;
  /** A small tile in away mode shows a still from its own stream. */
  still: boolean;
}

/**
 * Follow the server's hero choice over a list of cameras.
 *
 * `cameras()` is the current page; `source` is where the hero and its
 * events come from, so tests drive it without a server.
 */
export function createHero(cameras: () => Camera[], source: HeroSource = serverSource()) {
  let hero = $state<string | null>(null);
  let away = $state(false);

  const tiles = $derived.by((): Tile[] => {
    const list = [...cameras()];
    const at = hero === null ? -1 : list.findIndex((camera) => camera.id === hero);
    if (at > 0) {
      list.unshift(...list.splice(at, 1));
    }
    const lead = at >= 0 ? list[0].id : null;
    return list.map((camera) => ({
      camera,
      hero: camera.id === lead,
      still: away && camera.id !== lead,
    }));
  });

  /** Ask the server who is big. */
  async function refresh(): Promise<void> {
    try {
      hero = await source.hero();
    } catch {
      // The list's own error surface covers an unreachable server; a hero
      // is not worth a second banner.
    }
  }

  /** Follow the house: every event is a hint to re-ask. */
  function follow(): () => void {
    return source.watch(() => void refresh());
  }

  return {
    get tiles() {
      return tiles;
    },
    get hero() {
      return hero;
    },
    get away() {
      return away;
    },
    setAway(next: boolean) {
      away = next;
    },
    refresh,
    follow,
  };
}

/** The live source: the motion API and the server's event socket. */
export function serverSource(): HeroSource {
  return {
    hero: async () => (await api.hero()).hero ?? null,
    watch(onEvent) {
      let stopped = false;
      let socket: WebSocket | null = null;
      let again: ReturnType<typeof setTimeout> | null = null;
      const open = () => {
        if (stopped) return;
        socket = new WebSocket(`${location.origin.replace(/^http/, 'ws')}/api/events`);
        socket.onmessage = () => onEvent();
        // A dropped socket would silently freeze the hero: come back.
        socket.onclose = () => {
          if (stopped) return;
          again = setTimeout(open, 3000);
        };
      };
      open();
      return () => {
        // Teardown ends the reconnects too: closing the socket fires its
        // own `onclose`, and without this flag that would arm another one
        // and another, one dead socket per visit.
        stopped = true;
        if (again) clearTimeout(again);
        socket?.close();
        socket = null;
      };
    },
  };
}
