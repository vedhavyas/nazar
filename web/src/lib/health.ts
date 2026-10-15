//! The reachability poller behind the "server unreachable" banner.

import { writable } from 'svelte/store';

import { api } from './api';

/** False while the server is not answering at all. */
export const reachable = writable(true);

/** Poll `/api/health`; the returned function stops the poller. */
export function watchHealth(intervalMs = 5000): () => void {
  let stopped = false;
  const check = async () => {
    try {
      await api.health();
      reachable.set(true);
    } catch {
      reachable.set(false);
    }
  };
  void check();
  const timer = setInterval(() => {
    if (!stopped) void check();
  }, intervalMs);
  return () => {
    stopped = true;
    clearInterval(timer);
  };
}
