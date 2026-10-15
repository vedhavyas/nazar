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
