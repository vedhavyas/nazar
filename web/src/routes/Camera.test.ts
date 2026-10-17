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
  const mock = vi.fn(async (url: string, init?: RequestInit) => {
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
  });
  vi.stubGlobal('fetch', mock);
  return mock;
}

function open() {
  const onlocked = vi.fn();
  render(Camera, { id: 'cam-1', onback: vi.fn(), onlocked });
  return { onlocked };
}

test('the screen holds the player window while the server is waking', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  let state = 'waking';
  const mock = vi.fn(async (url: string) => {
    if (url === '/api/cameras') {
      return json({
        cameras: [{ id: 'cam-1', name: 'Hall', state, reason: null }],
        needsLogin: false,
      });
    }
    if (url.startsWith('/api/cameras/cam-1/live')) {
      return json({ stream: 'cam-1-high' });
    }
    return json({ privacy: false });
  });
  vi.stubGlobal('fetch', mock);
  const onlocked = vi.fn();
  render(Camera, { id: 'cam-1', onback: vi.fn(), onlocked });

  await vi.advanceTimersByTimeAsync(100);
  expect(document.querySelector('video-rtc'), 'the player is mounted').toBeTruthy();

  // The server keeps waking well past the player's window: nothing is
  // declared, and no fallback is negotiated for a stream with no picture.
  await vi.advanceTimersByTimeAsync(15000);
  expect(
    document.querySelector('script[src="/hls.min.js"]'),
    'the start window did not run against a waking stream',
  ).toBeNull();

  // The state arrives live: the window opens and, with no picture, fires.
  state = 'live';
  await vi.advanceTimersByTimeAsync(5000);
  await vi.advanceTimersByTimeAsync(13000);
  expect(
    document.querySelector('script[src="/hls.min.js"]'),
    'and it runs once the waking is over',
  ).toBeTruthy();
});

test('a stalled picture is asked for again on its own', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  vi.stubGlobal('Hls', undefined);
  const mock = stubServer();
  open();
  await vi.waitFor(() =>
    expect(document.querySelector('video-rtc'), 'the picture is mounted').toBeTruthy(),
  );

  const asked = () =>
    mock.mock.calls.filter(([url]) =>
      String(url).startsWith('/api/cameras/cam-1/live'),
    ).length;
  const before = asked();

  // The picture never starts (no first frame in the window) and the
  // fallback cannot take it: the screen re-asks on its own after the
  // first backoff step, with nobody clicking.
  await vi.advanceTimersByTimeAsync(16000);
  const scripts = document.querySelectorAll('script[src="/hls.min.js"]');
  expect(scripts.length, 'the fallback was attempted').toBeGreaterThan(0);
  scripts.item(scripts.length - 1)?.dispatchEvent(new Event('error'));

  await vi.advanceTimersByTimeAsync(1100);
  expect(asked(), 'the screen re-asks the server on its own').toBe(before + 1);
});

test('the picture arrives muted, and the speaker on it is what unmutes it', async () => {
  vi.useFakeTimers();
  stubServer();
  open();
  await vi.advanceTimersByTimeAsync(600);

  const video = document.querySelector('video-rtc video') as HTMLVideoElement | null;
  expect(video, 'the player is on the stage').toBeTruthy();
  expect(video?.muted, 'nothing plays sound before a gesture').toBe(true);

  const speaker = screen.getByTestId('speaker');
  expect(speaker.getAttribute('aria-pressed'), 'the speaker starts off').toBe('false');

  speaker.click();
  await vi.advanceTimersByTimeAsync(600);

  expect(video?.muted, 'the toggle is the gesture that unmutes').toBe(false);
  expect(speaker.getAttribute('aria-pressed'), 'and the speaker reads on').toBe('true');
  expect(
    localStorage.getItem('nazar.audio.cam-1'),
    'the choice is remembered per camera',
  ).toBe('1');
});

test('a remembered choice starts the camera unmuted', async () => {
  vi.useFakeTimers();
  localStorage.setItem('nazar.audio.cam-1', '1');
  stubServer();
  open();
  await vi.advanceTimersByTimeAsync(600);

  const video = document.querySelector('video-rtc video') as HTMLVideoElement | null;
  expect(video, 'the player is on the stage').toBeTruthy();
  expect(video?.muted, 'the remembered choice is honored').toBe(false);
  expect(screen.getByTestId('speaker').getAttribute('aria-pressed')).toBe('true');
});

test('the old Listen button is gone', async () => {
  stubServer();
  open();
  await vi.waitFor(() =>
    expect(document.querySelector('video-rtc'), 'the picture is up').toBeTruthy(),
  );

  expect(
    screen.queryByRole('button', { name: /^(Listen|Listening)$/ }),
    'the separate Listen button is removed entirely',
  ).toBeNull();
});

test('a sustained drag lands every window, one write at a time', async () => {
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

/** Hide or show the page, as a browser tab switch does. */
function setHidden(hidden: boolean) {
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  Object.defineProperty(document, 'visibilityState', {
    configurable: true,
    get: () => (hidden ? 'hidden' : 'visible'),
  });
  document.dispatchEvent(new Event('visibilitychange'));
}

test('a tab that comes back asks the server for the stream again', async () => {
  const fetchMock = stubServer();
  open();

  await vi.waitFor(() => expect(document.querySelector('video-rtc video')).toBeTruthy());
  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 4;

  // Away, and the peer drops while we are not looking: the clock is where
  // it was on return. A hidden camera may outlive its server stream, so
  // the re-ask must reach the server, not just rebuild the player.
  setHidden(true);
  setHidden(false);

  await vi.waitFor(() => {
    expect(
      fetchMock.mock.calls.some(([url]) =>
        String(url).includes('/api/cameras/cam-1/live?quality=high&retry=true'),
      ),
      'the stream is asked for again',
    ).toBe(true);
  });
  expect(document.querySelectorAll('video-rtc').length, 'one player, still').toBe(1);
});

test('a background re-ask that fails raises no banner', async () => {
  // The first ask answers; the one after coming back does not - the server
  // is mid-restart, say. The tile path lets the same failure pass: the
  // picture's own stall is what tells the person, and a banner outliving
  // it would read as the screen being broken.
  const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
    if (url === '/api/cameras') {
      return json({
        cameras: [{ id: 'cam-1', name: 'Hall', state: 'live', reason: null }],
        needsLogin: false,
      });
    }
    if (url.startsWith('/api/cameras/cam-1/live')) {
      if (url.includes('retry=true')) {
        return new Response(JSON.stringify({ error: 'server is restarting' }), {
          status: 502,
        });
      }
      return json({ stream: 'cam-1-high' });
    }
    if (url === '/api/cameras/cam-1/privacy' && init?.method !== 'POST') {
      return json({ privacy: false });
    }
    return json({});
  });
  vi.stubGlobal('fetch', fetchMock);
  open();

  await vi.waitFor(() => expect(document.querySelector('video-rtc video')).toBeTruthy());
  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 4;

  setHidden(true);
  setHidden(false);

  await vi.waitFor(() => {
    expect(
      fetchMock.mock.calls.some(([url]) =>
        String(url).includes('/api/cameras/cam-1/live?quality=high&retry=true'),
      ),
      'the re-ask went out',
    ).toBe(true);
  });
  // Let the refusal land before reading the screen.
  await new Promise((resolve) => setTimeout(resolve, 30));
  expect(
    screen.queryByRole('alert'),
    'the failed background re-ask stays off the banner',
  ).toBeNull();
});
