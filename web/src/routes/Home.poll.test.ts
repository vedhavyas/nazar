//! The poll and the players: an unchanged house (the same cameras, the same
//! streams, the same states) must not disturb what is on the stage. The
//! acceptance caught the other face of this - a house whose poll churn
//! remounted every player on every tick, so none lived long enough to say
//! the picture was missing.

// @vitest-environment jsdom

import { cleanup, render } from '@testing-library/svelte';
import { afterEach, beforeAll, expect, test, vi } from 'vitest';

import Home from './Home.svelte';

class StubPlayer extends HTMLElement {
  connectedCallback() {
    this.appendChild(document.createElement('video'));
  }
}

beforeAll(() => {
  if (!customElements.get('video-rtc')) {
    customElements.define('video-rtc', StubPlayer);
  }
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  localStorage.clear();
});

test('an unchanged poll response never remounts a player', async () => {
  vi.useFakeTimers();
  const cameras = [
    { id: 'cam-1', name: 'Hall', state: 'waking', reason: null },
    { id: 'cam-2', name: 'Door', state: 'degraded', reason: 'the producer has not connected' },
  ];
  // Every poll answers equal data in fresh objects, the way the server's
  // JSON decode does.
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      const u = String(url);
      const body = u === '/api/cameras'
        ? { cameras: cameras.map((c) => ({ ...c })), needsLogin: false }
        : u.includes('/api/motion/hero')
          ? { hero: null }
          : u.includes('/live')
            ? { stream: `${u.split('/api/cameras/')[1].split('/')[0]}-low` }
            : {};
      return new Response(JSON.stringify(body), { status: 200 });
    }),
  );
  render(Home, { onlocked: vi.fn(), onopen: vi.fn() });

  await vi.advanceTimersByTimeAsync(500);
  const first = document.querySelectorAll('video-rtc');
  expect(first.length, 'the waking camera has its player').toBeGreaterThan(0);
  const firstPlayer = first[0];

  // Several poll ticks with identical data.
  await vi.advanceTimersByTimeAsync(10_000);
  const after = document.querySelectorAll('video-rtc');
  expect(after[0], 'the same player survives an unchanged poll').toBe(firstPlayer);
});
