//! The camera screen's quiet defaults: the picture arrives muted, and the
//! lens state on arrival is the camera's own answer.

// @vitest-environment jsdom

import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, beforeAll, expect, test, vi } from 'vitest';

import Camera from './Camera.svelte';

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
  vi.unstubAllGlobals();
  vi.useRealTimers();
  localStorage.clear();
});

function json(body: unknown): Response {
  return new Response(JSON.stringify(body), { status: 200 });
}

/** The server as this screen uses it: cameras, a stream, a lens state. */
function stubServer(privacy: boolean | null = false) {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string, init?: RequestInit) => {
      if (url === '/api/cameras') {
        return json({
          cameras: [{ id: 'cam-1', name: 'Hall', state: 'live', reason: null }],
          needsLogin: false,
        });
      }
      if (url.startsWith('/api/cameras/cam-1/live')) {
        return json({ stream: 'cam-1-high' });
      }
      if (url === '/api/cameras/cam-1/privacy' && init?.method !== 'POST') {
        return json({ privacy });
      }
      return json({});
    }),
  );
}

function open() {
  const onlocked = vi.fn();
  render(Camera, { id: 'cam-1', onback: vi.fn(), onlocked });
  return { onlocked };
}

test('the picture arrives muted, and the toggle is what unmutes it', async () => {
  vi.useFakeTimers();
  stubServer();
  open();
  await vi.advanceTimersByTimeAsync(600);

  const video = document.querySelector('video-rtc video') as HTMLVideoElement | null;
  expect(video, 'the player is on the stage').toBeTruthy();
  expect(video?.muted, 'nothing plays sound before a gesture').toBe(true);

  screen.getByRole('button', { name: 'Listen' }).click();
  await vi.advanceTimersByTimeAsync(600);

  expect(video?.muted, 'the toggle is the gesture that unmutes').toBe(false);
  expect(screen.getByRole('button', { name: 'Listening' }), 'and it says so').toBeTruthy();
});

test('a dragged move waits its turn, and only the latest one queues', async () => {
  const calls: string[] = [];
  const releases: Array<() => void> = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/ptz')) {
        calls.push(String(init?.body));
        await new Promise<void>((resolve) => releases.push(resolve));
        return json({ ok: true, position: { x: 0, y: 0 } });
      }
      if (url === '/api/cameras') {
        return json({
          cameras: [{ id: 'cam-1', name: 'Hall', state: 'live', reason: null }],
          needsLogin: false,
        });
      }
      if (url.startsWith('/api/cameras/cam-1/live')) {
        return json({ stream: 'cam-1-high' });
      }
      if (url === '/api/cameras/cam-1/privacy') {
        return json({ privacy: false });
      }
      return json({});
    }),
  );
  open();
  await vi.waitFor(() =>
    expect(document.querySelector('video-rtc'), 'the picture is up').toBeTruthy(),
  );

  const surface = screen.getByTestId('ptz-surface');
  // Three drag windows while the first move is still waiting on the
  // camera: every window's motion must survive, one write at a time.
  vi.useFakeTimers();
  surface.dispatchEvent(new MouseEvent('pointerdown', { clientX: 0, clientY: 0, bubbles: true }));
  for (const at of [200, 400, 600]) {
    surface.dispatchEvent(
      new MouseEvent('pointermove', { clientX: at, clientY: 0, bubbles: true }),
    );
    await vi.advanceTimersByTimeAsync(300);
  }
  vi.useRealTimers();

  expect(calls.length, 'one move is in flight, not three').toBe(1);
  expect(calls[0], 'the first window, in the camera signs').toContain('"dx":-60');

  releases[0]();
  await vi.waitFor(() =>
    expect(calls.length, 'the queued window is sent next').toBe(2),
  );
  expect(calls[1], 'the second window queues, not replaces').toContain('"dx":-60');
  releases[1]();
  await vi.waitFor(() =>
    expect(calls.length, 'and the third after it').toBe(3),
  );
  releases[2]();
  await vi.waitFor(() =>
    expect(screen.getByText(/at 0,0/), 'the camera answers the last one').toBeTruthy(),
  );

  const moved = calls.reduce((sum, body) => sum + Number(JSON.parse(body).dx), 0);
  expect(
    moved,
    'every window of the drag landed: three windows, 180 units, no loss',
  ).toBe(-180);
});

test('the lens state is the camera’s own answer', async () => {
  stubServer(true);
  open();

  expect(
    await screen.findByRole('button', { name: 'Privacy on' }),
    'a closed lens reads as privacy on before anything is touched',
  ).toBeTruthy();
});

test('a lens nobody reports reads as neither on nor off', async () => {
  stubServer(null);
  open();

  expect(await screen.findByRole('button', { name: 'Privacy' })).toBeTruthy();
});
