//! The recordings screen: the day's clips render as the server told them,
//! a scrub re-mints at the position it lands on, a tab that comes back
//! re-mints the same instant, and the exit releases the session.

// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeAll, expect, test, vi } from 'vitest';

import ClipPlayer, { timestampAt } from './ClipPlayer.svelte';
import Recordings from '../routes/Recordings.svelte';

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
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

/** Midnight IST, 16 October 2026: the day these tests browse. */
const DAY_START = 1_792_089_000_000;
const HOUR = 3_600_000;

function json(body: unknown): Response {
  return new Response(JSON.stringify(body), { status: 200 });
}

/** The server as this screen uses it, recording every call. */
function stubServer() {
  const calls: string[] = [];
  const mock = vi.fn(async (url: string, init?: RequestInit) => {
    calls.push(`${init?.method ?? 'GET'} ${url}`);
    if (url === '/api/cameras') {
      return json({
        cameras: [{ id: 'cam-1', name: 'Hall', state: 'live', reason: null }],
        needsLogin: false,
      });
    }
    if (url.startsWith('/api/cameras/cam-1/recordings/days')) {
      return json({ days: ['15-10-2026', '16-10-2026'] });
    }
    if (url.startsWith('/api/cameras/cam-1/recordings?day=16-10-2026')) {
      return json({
        clips: [
          { id: 'c2', start_ms: DAY_START + 7 * HOUR + 30 * 60_000, snapshot_url: '/api/snaps/c2' },
          { id: 'c1', start_ms: DAY_START + 2 * HOUR, snapshot_url: null },
        ],
      });
    }
    if (url === '/api/cameras/cam-1/playback' && init?.method === 'POST') {
      return json({ stream: 'vod-cam-1', expires_in: 120 });
    }
    if (url === '/api/cameras/cam-1/playback' && init?.method === 'DELETE') {
      return new Response(null, { status: 204 });
    }
    return json({});
  });
  vi.stubGlobal('fetch', mock);
  return { mock, calls };
}

test("the day's clips render as the server told them", async () => {
  const { calls } = stubServer();
  render(Recordings, { id: 'cam-1', onback: vi.fn(), onlocked: vi.fn() });

  // The newest recorded day is what the screen opens on, and its clips
  // read as the household's clock.
  expect(await screen.findByText('07:30'), 'the newest day, newest first').toBeTruthy();
  expect(screen.getByText('02:00'), 'and the rest of the day under it').toBeTruthy();
  expect(
    calls.some((call) => call.includes('GET /api/cameras/cam-1/recordings?day=16-10-2026')),
    'the newest day was the one asked for',
  ).toBe(true);

  const stills = document.querySelectorAll('img');
  expect(stills.length, 'the clip with a still carries it').toBe(1);
  expect(stills[0].getAttribute('src')).toBe('/api/snaps/c2');
});

test('a scrub re-mints at the position it lands on', async () => {
  const onseek = vi.fn();
  render(ClipPlayer, {
    stream: 'vod-cam-1',
    dayStart: DAY_START,
    at: DAY_START,
    onseek,
    onexit: vi.fn(),
  });

  expect(timestampAt(DAY_START, 0.5), 'halfway through the day').toBe(DAY_START + 12 * HOUR);

  const scrub = screen.getByTestId('scrub') as HTMLInputElement;
  await fireEvent.change(scrub, { target: { value: '500' } });

  expect(onseek, 'the seek lands on the instant the thumb points at').toHaveBeenCalledWith(
    DAY_START + 12 * HOUR,
  );
});

/** The player the component renders, once it grew a video. */
async function videoOf(): Promise<HTMLVideoElement> {
  await vi.waitFor(() => expect(document.querySelector('video-rtc video')).toBeTruthy());
  return document.querySelector('video-rtc video') as HTMLVideoElement;
}

/** Fail the vendored player's fetch, as a browser that cannot reach it
 * would: the retry path is what is left. */
async function failVendoredPlayer() {
  const scripts = document.querySelectorAll('script[src="/hls.min.js"]');
  const script = scripts.item(scripts.length - 1);
  expect(script, 'the player is only fetched when it is needed').toBeTruthy();
  script?.dispatchEvent(new Event('error'));
}

test('a paused clip is not a stalled one', async () => {
  vi.useFakeTimers();
  // Native HLS: a stall here would silently swap to the fallback, which is
  // exactly the symptom a deliberate pause must not produce.
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('maybe');
  const onseek = vi.fn();
  render(ClipPlayer, {
    stream: 'vod-cam-1',
    dayStart: DAY_START,
    at: DAY_START + 3 * HOUR,
    onseek,
    onexit: vi.fn(),
  });

  const video = await videoOf();
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 12;
  await vi.advanceTimersByTimeAsync(5000);

  video.dispatchEvent(new Event('pause'));
  await vi.advanceTimersByTimeAsync(30_000);

  expect(
    document.querySelector('[data-testid=hls]'),
    'a paused clip never falls back: the clock stands still on purpose',
  ).toBeNull();
  expect(onseek, 'and never re-mints').not.toHaveBeenCalled();

  // Resumed, the watch picks the picture back up where it stands.
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 13;
  await vi.advanceTimersByTimeAsync(5000);
  expect(onseek, 'a moving picture raises nothing').not.toHaveBeenCalled();
  expect(document.querySelector('[data-testid=hls]'), 'and stays on the socket').toBeNull();
});

test('the retry re-mints where the clock stood, not where the clip began', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  const at = DAY_START + 3 * HOUR;
  const onseek = vi.fn();
  render(ClipPlayer, {
    stream: 'vod-cam-1',
    dayStart: DAY_START,
    at,
    onseek,
    onexit: vi.fn(),
  });

  const video = await videoOf();
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 42;
  await vi.advanceTimersByTimeAsync(5000);
  // The clock stops: no advance across two samples.
  await vi.advanceTimersByTimeAsync(11_000);
  await failVendoredPlayer();
  await vi.waitFor(() => expect(screen.getByText(/may have ended/)).toBeTruthy());

  screen.getByText('Try again').click();
  expect(
    onseek,
    'the re-ask lands at the playhead the picture died at, not the mint point',
  ).toHaveBeenCalledWith(at + 42_000);
});

/** Hide or show the page, as a browser tab switch does. */
function setHidden(hidden: boolean) {
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  Object.defineProperty(document, 'visibilityState', {
    configurable: true,
    get: () => (hidden ? 'hidden' : 'visible'),
  });
  document.dispatchEvent(new Event('visibilitychange'));
}

test('a tab that comes back asks for the session again', async () => {
  const onseek = vi.fn();
  const at = DAY_START + 3 * HOUR;
  render(ClipPlayer, {
    stream: 'vod-cam-1',
    dayStart: DAY_START,
    at,
    onseek,
    onexit: vi.fn(),
  });

  await vi.waitFor(() => expect(document.querySelector('video-rtc video')).toBeTruthy());
  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 4;

  // Away, and the session dies while nobody consumes it: the cloud drops
  // one long before this server's own idle window, so the re-ask is the
  // only thing that brings the picture back.
  setHidden(true);
  setHidden(false);

  await vi.waitFor(() => {
    expect(
      onseek,
      'the playhead the picture stood at is minted again, not the clip start',
    ).toHaveBeenCalledWith(at + 4000);
  });
});

test('the exit releases the session', async () => {
  const { calls } = stubServer();
  const onback = vi.fn();
  render(Recordings, { id: 'cam-1', onback, onlocked: vi.fn() });

  (await screen.findByText('07:30')).click();
  await vi.waitFor(() => {
    expect(
      calls.some((call) => call.startsWith('POST /api/cameras/cam-1/playback')),
      'the clip starts a playback session',
    ).toBe(true);
  });

  // Wait for the player itself: the call was recorded before the answer
  // landed, so the button is what says the session is really up.
  (await screen.findByRole('button', { name: 'Close' })).click();
  await vi.waitFor(() => {
    expect(
      calls.some((call) => call.startsWith('DELETE /api/cameras/cam-1/playback')),
      'the player leaving releases it',
    ).toBe(true);
  });
});
