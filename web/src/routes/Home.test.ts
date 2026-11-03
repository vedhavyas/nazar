//! Home's routing half of the needs-login signal, and the quality classes
//! that follow the hero.

// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, expect, test, vi } from 'vitest';

import Home from './Home.svelte';

// The real tile never renders a camera's detail, so the Home tests read
// the props it is handed through a stand-in.
vi.mock('../lib/CameraTile.svelte', async () => ({
  default: (await import('../lib/CameraTileStub.svelte')).default,
}));

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

test('a refused session routes Home back to login', async () => {
  vi.stubGlobal(
    'fetch',
    vi.fn(
      async () =>
        new Response(JSON.stringify({ cameras: [], needsLogin: true }), { status: 200 }),
    ),
  );
  const onlocked = vi.fn();

  render(Home, { onlocked, onopen: vi.fn() });

  await waitFor(() => expect(onlocked, 'Home hands the browser back to login').toHaveBeenCalled());
});

function json(body: unknown): Response {
  return new Response(JSON.stringify(body), { status: 200 });
}

/** The stream asks a page made, in order. */
function asked(calls: string[]): string[] {
  return calls.filter((call) => call.includes('/live?'));
}

test('a new hero is asked high, and the old one drops to low', async () => {
  vi.useFakeTimers();
  let hero = 'hall';
  const calls: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string, init?: RequestInit) => {
      calls.push(`${init?.method ?? 'GET'} ${url}`);
      if (url === '/api/cameras') {
        return json({
          cameras: [
            { id: 'hall', name: 'Hall', state: 'live', reason: null },
            { id: 'gate', name: 'Gate', state: 'live', reason: null },
          ],
          needsLogin: false,
        });
      }
      if (url === '/api/motion/hero') {
        return json({ hero });
      }
      const stream = url.replace('/api/cameras/', '').split('/live')[0];
      return json({ stream: `${stream}-stream` });
    }),
  );

  render(Home, { onlocked: vi.fn(), onopen: vi.fn() });
  await vi.advanceTimersByTimeAsync(50);

  expect(
    asked(calls),
    'the hero streams high and the tile low from the first pass',
  ).toEqual([
    'POST /api/cameras/hall/live?quality=high',
    'POST /api/cameras/gate/live?quality=low',
  ]);

  // The house moves: the server elects a new hero, and the next poll hears it.
  hero = 'gate';
  calls.length = 0;
  await vi.advanceTimersByTimeAsync(3500);

  expect(
    asked(calls),
    'the promoted camera is re-asked high and the demoted one low',
  ).toEqual([
    'POST /api/cameras/gate/live?quality=high',
    'POST /api/cameras/hall/live?quality=low',
  ]);
});

test('a settled camera is not re-asked', async () => {
  vi.useFakeTimers();
  const calls: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string, init?: RequestInit) => {
      calls.push(`${init?.method ?? 'GET'} ${url}`);
      if (url === '/api/cameras') {
        return json({
          cameras: [{ id: 'hall', name: 'Hall', state: 'live', reason: null }],
          needsLogin: false,
        });
      }
      if (url === '/api/motion/hero') {
        return json({ hero: 'hall' });
      }
      return json({ stream: 'hall-high' });
    }),
  );

  render(Home, { onlocked: vi.fn(), onopen: vi.fn() });
  await vi.advanceTimersByTimeAsync(50);
  expect(asked(calls).length, 'one camera, one ask').toBe(1);

  // The camera comes up and stays the hero: the poll must not hammer it.
  calls.length = 0;
  await vi.advanceTimersByTimeAsync(1000);
  expect(
    asked(calls),
    'a live stream of the right class is left alone',
  ).toEqual([]);
});

test('a retile drops the stale reason and detail', async () => {
  vi.useFakeTimers();
  const detail =
    'go2rtc api: error sending request for url (http://127.0.0.1:1984/api/streams?<redacted>)';
  let hang = false;
  const calls: string[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      calls.push(String(url));
      if (url === '/api/cameras') {
        if (hang) return new Promise<Response>(() => {});
        return json({
          cameras: [
            {
              id: 'hall',
              name: 'Hall',
              state: 'degraded',
              reason: 'stream engine unavailable',
              details: detail,
            },
          ],
          needsLogin: false,
        });
      }
      if (url === '/api/motion/hero') {
        return json({ hero: null });
      }
      return json({ stream: 'hall-low' });
    }),
  );

  render(Home, { onlocked: vi.fn(), onopen: vi.fn() });
  // The first watch marks the camera waking; the next poll restores the
  // server's degraded answer.
  await vi.advanceTimersByTimeAsync(3500);
  const shown = () => JSON.parse(screen.getByTestId('camera').textContent ?? '{}');
  expect(shown().details, 'the tile carries the server detail').toBe(detail);

  // The poll after the retile never answers, so the tile's own state is
  // what remains on screen.
  hang = true;
  screen.getByText('retile').click();
  await vi.advanceTimersByTimeAsync(100);

  expect(shown().state, 'the retile asks for a fresh stream').toBe('waking');
  expect(shown().reason, 'and drops the old reason').toBe(null);
  expect(shown().details, 'and the detail with it').toBe(null);
  const retiled = calls.filter((url) => url.includes('/live?')).pop();
  expect(retiled, 'the retile pulls the mint forward').toContain('retry=true');
});
