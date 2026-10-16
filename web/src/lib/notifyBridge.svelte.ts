//! The bridge between the server's notifications and this machine: what to
//! say about an entry, how many are unread, the socket that brings arrivals
//! in, and the telling itself - the shell's own `notify` when there is a
//! shell, else the browser's notifications when they are permitted.

import { api, type Notification } from './api';

/** Where entries, camera names, and arrivals come from; a server, or a fake
 * in tests. */
export interface NotifySource {
  /** Every entry the server still holds, newest first. */
  list(): Promise<Notification[]>;
  /** The camera names device ids map to, for the list's words. */
  names(): Promise<Record<string, string>>;
  /** Follow arrivals. Returns a teardown. */
  watch(onNew: (entry: Notification) => void): () => void;
}

/** The live source: the notifications API and the server's event socket. */
export function serverNotifySource(): NotifySource {
  return {
    list: async () => (await api.notifications()).notifications,
    names: async () => {
      const page = await api.cameras();
      return Object.fromEntries(page.cameras.map((camera) => [camera.id, camera.name]));
    },
    watch(onNew) {
      let stopped = false;
      let socket: WebSocket | null = null;
      let again: ReturnType<typeof setTimeout> | null = null;
      const open = () => {
        if (stopped) return;
        socket = new WebSocket(`${location.origin.replace(/^http/, 'ws')}/api/events`);
        socket.onmessage = (message: MessageEvent<string>) => {
          const frame = JSON.parse(String(message.data)) as {
            type?: string;
            notification?: Notification;
          };
          // The socket carries every event; only the told ones are ours.
          if (frame.type === 'notification' && frame.notification) {
            onNew(frame.notification);
          }
        };
        socket.onclose = () => {
          if (stopped) return;
          again = setTimeout(open, 3000);
        };
      };
      open();
      return () => {
        // Teardown ends the reconnects too; without the flag the socket's
        // own `onclose` would arm another one, one dead socket per visit.
        stopped = true;
        if (again) clearTimeout(again);
        socket?.close();
        socket = null;
      };
    },
  };
}

/** The entry in words: the camera's name when the device is a camera. */
export function describe(entry: Notification, names: Record<string, string>): string {
  const who = names[entry.device_id];
  switch (entry.kind) {
    case 'motion':
      return who ? `Motion at ${who}` : 'Motion';
    case 'offline':
      return who ? `${who} went offline` : 'A device went offline';
    case 'online':
      return who ? `${who} is back online` : 'A device is back online';
    case 'aqi':
      return who ? `Air quality ${entry.value} at ${who}` : `Air quality ${entry.value}`;
  }
}

/** A short way to say when, for the list. */
export function since(at: number, now: number): string {
  const secs = Math.max(0, Math.floor((now - at) / 1000));
  if (secs < 60) return 'just now';
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

/** The shell's invoke, when this page runs inside one (Tauri). */
function shellInvoke(): Invoke | null {
  const shell = window as unknown as {
    __TAURI_INTERNALS__?: { invoke?: Invoke };
    __TAURI__?: { invoke?: Invoke; core?: { invoke?: Invoke } };
  };
  return (
    shell.__TAURI_INTERNALS__?.invoke ?? shell.__TAURI__?.core?.invoke ?? shell.__TAURI__?.invoke ?? null
  );
}

/** Tell the person about something, outside the page. */
export async function notify(title: string, body: string): Promise<'shell' | 'browser' | 'none'> {
  const invoke = shellInvoke();
  if (invoke) {
    try {
      await invoke('notify', { title, body });
      return 'shell';
    } catch {
      // A shell may not carry the notify command yet: fall through to the
      // browser rather than dropping the telling.
    }
  }
  // The DOM's own Notification, beside this module's wire type of the same
  // name.
  if (typeof window.Notification !== 'undefined' && window.Notification.permission === 'granted') {
    new window.Notification(title, body ? { body } : undefined);
    return 'browser';
  }
  return 'none';
}

/** Told about an arrival, outside the page. */
export type Tell = (entry: Notification, names: Record<string, string>) => void;

export const SEEN_KEY = 'nazar.notifications.seen';

function readSeen(): number {
  try {
    return Number(localStorage.getItem(SEEN_KEY)) || 0;
  } catch {
    return 0;
  }
}

/** Everything the bell and the list read. */
export function createNotifications(
  source: NotifySource = serverNotifySource(),
  tell: Tell = (entry, names) => void notify(describe(entry, names), ''),
) {
  let entries = $state<Notification[]>([]);
  let names = $state<Record<string, string>>({});
  let seenAt = $state(readSeen());

  const unread = $derived(entries.filter((entry) => entry.at > seenAt).length);

  /** The server's list and the camera names; an unreachable server is the
   * health banner's to report. */
  async function load(): Promise<void> {
    try {
      const [list, cameraNames] = await Promise.all([source.list(), source.names()]);
      // An arrival that landed while the list was in flight is newer than
      // anything the snapshot carries: it stays in front rather than being
      // overwritten by the answer.
      const seen = new Set(list.map((entry) => entry.id));
      entries = [...entries.filter((entry) => !seen.has(entry.id)), ...list];
      names = cameraNames;
    } catch {
      // Covered by the banner; the bell just stays quiet.
    }
  }

  /** Follow arrivals. Returns a teardown. */
  function follow(): () => void {
    return source.watch((entry) => {
      entries = [entry, ...entries.filter((old) => old.id !== entry.id)];
      tell(entry, names);
    });
  }

  /** Everything up to now has been looked at. */
  function markSeen(): void {
    const newest = entries.reduce((at, entry) => Math.max(at, entry.at), 0);
    if (newest <= seenAt) return;
    seenAt = newest;
    try {
      localStorage.setItem(SEEN_KEY, String(newest));
    } catch {
      // A browser that refuses storage just forgets the look.
    }
  }

  return {
    get entries() {
      return entries;
    },
    get names() {
      return names;
    },
    get unread() {
      return unread;
    },
    load,
    follow,
    markSeen,
  };
}
