//! The reachability poller behind the "server unreachable" banner.

import { get } from 'svelte/store';
import { afterEach, expect, test, vi } from 'vitest';

import { reachable, watchHealth } from './health';

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

test('the banner follows the server in both directions', async () => {
  vi.useFakeTimers();
  let healthy = false;
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => {
      if (!healthy) {
        throw new TypeError('fetch failed');
      }
      return new Response(JSON.stringify({ ok: true, version: '1' }), { status: 200 });
    }),
  );

  const stop = watchHealth(1000);
  await vi.advanceTimersByTimeAsync(0);
  expect(get(reachable), 'a failing poll marks the server unreachable').toBe(false);

  healthy = true;
  await vi.advanceTimersByTimeAsync(1000);
  expect(get(reachable), 'the next answered poll clears the banner').toBe(true);

  stop();
  healthy = false;
  await vi.advanceTimersByTimeAsync(2000);
  expect(get(reachable), 'a stopped poller stops touching the store').toBe(true);
});
