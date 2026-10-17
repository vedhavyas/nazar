//! The tile's liveness watch: the no-first-frame stall, the frozen picture,
//! and the retry both offer.

// @vitest-environment jsdom

import { cleanup, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeAll, expect, test, vi } from 'vitest';

import CameraTile from './CameraTile.svelte';
import TileHost from './TileHost.svelte';

/** The vendored player's contract, in miniature: it grows a video on
 * connect, which is the element the tile watches. */
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
  localStorage.clear();
  // The player script lands in <head>, which cleanup does not empty.
  for (const script of document.querySelectorAll('script[src="/hls.min.js"]')) {
    script.remove();
  }
});

test('a socket that cannot negotiate falls back to HLS where it plays', async () => {
  vi.useFakeTimers();
  // This browser plays HLS by itself: Safari, iOS, Chrome on Android.
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('maybe');
  liveTile();

  expect(
    document.querySelector('[data-testid=hls]'),
    'no fallback before the window is out',
  ).toBeNull();

  await vi.advanceTimersByTimeAsync(12_500);

  const fallback = document.querySelector('[data-testid=hls]');
  expect(fallback, 'the compatibility stream took over').toBeTruthy();
  expect(
    fallback?.getAttribute('src'),
    "pointed at this tile's own stream",
  ).toBe('/go2rtc/api/stream.m3u8?src=cam-1-high');
});

test('a browser without native HLS gets the vendored player', async () => {
  vi.useFakeTimers();
  // Desktop Chromium plays no HLS itself, but hls.js carries it.
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  FakeHls.instances = [];
  vi.stubGlobal('Hls', FakeHls);
  liveTile();

  await vi.advanceTimersByTimeAsync(12_500);
  await vi.waitFor(() =>
    expect(FakeHls.instances.length, 'the vendored player is started').toBe(1),
  );

  const player = FakeHls.instances[0];
  expect(
    player.loaded[0],
    "the instance was pointed at the tile's stream",
  ).toBe('/go2rtc/api/stream.m3u8?src=cam-1-high');
  expect(player.attached[0]?.tagName, 'and given the element to play into').toBe('VIDEO');
  expect(
    document.querySelector('[data-testid=hls]'),
    'the fallback is what is shown',
  ).toBeTruthy();
  expect(screen.queryByText(/did not start/), 'no retry note').toBeNull();
});

/** The vendored player's shape: a class whose only static is
 * `isSupported`, with the rest on the instance - exactly as hls.js ships.
 * A stub shaped any other way would pass while the real file threw. */
class FakeHls {
  static instances: FakeHls[] = [];
  static isSupported = () => true;
  loaded: string[] = [];
  attached: HTMLVideoElement[] = [];
  destroyed = 0;
  constructor() {
    FakeHls.instances.push(this);
  }
  loadSource(source: string) {
    this.loaded.push(source);
  }
  attachMedia(video: HTMLVideoElement) {
    this.attached.push(video);
  }
  destroy() {
    this.destroyed += 1;
  }
}

/** Fail the vendored player's fetch, as a browser that cannot reach it
 * would: the retry path is what is left. */
async function failVendoredPlayer(note: RegExp = /did not start/) {
  const scripts = document.querySelectorAll('script[src="/hls.min.js"]');
  const script = scripts.item(scripts.length - 1);
  expect(script, 'the player is only fetched when it is needed').toBeTruthy();
  script?.dispatchEvent(new Event('error'));
  await vi.waitFor(() => {
    expect(screen.getByText(note), 'the person gets the retry').toBeTruthy();
  });
}

test('hls.js refusing the browser offers the retry too', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  FakeHls.instances = [];
  FakeHls.isSupported = () => false;
  vi.stubGlobal('Hls', FakeHls);
  liveTile();

  await vi.advanceTimersByTimeAsync(12_500);
  await vi.waitFor(() => {
    expect(screen.getByText(/did not start/), 'no player means the retry').toBeTruthy();
  });
  expect(
    document.querySelector('[data-testid=hls]'),
    'and no video element pretending to play',
  ).toBeNull();
});

test('a browser with neither native HLS nor hls.js offers the retry', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  vi.stubGlobal('Hls', undefined);
  liveTile();

  await vi.advanceTimersByTimeAsync(12_500);
  await failVendoredPlayer();

  expect(document.querySelector('[data-testid=hls]'), 'and no broken player').toBeNull();
});

function liveTile() {
  const onwatch = vi.fn();
  render(CameraTile, {
    camera: { id: 'cam-1', name: 'Hall', state: 'live', reason: null },
    stream: 'cam-1-high',
    onwatch,
  });
  return { onwatch };
}

test('the player asks for the stream the server minted', async () => {
  liveTile();

  const player = document.querySelector('video-rtc');
  expect(player, 'a live tile holds a player').toBeTruthy();
  expect(
    (player as HTMLElement & { src?: string }).src,
    'the source is the minted <device>-<quality> name, never a bare device id',
  ).toBe('/go2rtc/api/ws?src=cam-1-high');
});

test('a tile with no minted stream yet holds no player', () => {
  const onwatch = vi.fn();
  render(CameraTile, {
    camera: { id: 'cam-1', name: 'Hall', state: 'live', reason: null },
    onwatch,
  });

  expect(
    document.querySelector('video-rtc'),
    'nothing is asked for until the server has named the stream',
  ).toBeNull();
});

test('away mode shows a still over the tile it already streams', async () => {
  const onwatch = vi.fn();
  render(CameraTile, {
    camera: { id: 'cam-1', name: 'Hall', state: 'live', reason: null },
    stream: 'cam-1-low',
    onwatch,
    still: true,
  });

  expect(
    document.querySelector('canvas.still'),
    'the tile shows a still rather than the live picture',
  ).toBeTruthy();
  expect(
    document.querySelector('.picture.hidden'),
    'the live picture is hidden, not stopped',
  ).toBeTruthy();
  expect(
    document.querySelector('video-rtc'),
    'the tile keeps its own stream: the still comes from it, with no server work added',
  ).toBeTruthy();
});

test('a waking tile keeps its player mounted and shows a spinner', () => {
  const onwatch = vi.fn();
  render(CameraTile, {
    camera: { id: 'cam-1', name: 'Hall', state: 'waking', reason: null },
    stream: 'cam-1-low',
    onwatch,
  });

  expect(
    document.querySelector('video-rtc'),
    'the waking tile keeps its player: unmounting and remounting is the flip that restarted the picture',
  ).toBeTruthy();
  expect(
    document.querySelector('.spinner'),
    'and a spinner says it is still starting',
  ).toBeTruthy();
});

test('a waking tile does not run the player start window', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  const onwatch = vi.fn();
  render(CameraTile, {
    camera: { id: 'cam-1', name: 'Hall', state: 'waking', reason: null },
    stream: 'cam-1-low',
    onwatch,
  });

  await vi.advanceTimersByTimeAsync(20000);
  expect(
    document.querySelector('script[src="/hls.min.js"]'),
    'no fallback was ever reached: the start window never ran while the server was waking',
  ).toBeNull();
  expect(
    screen.queryByText(/did not start/),
    'and nothing was declared a failure for a stream the server is still bringing up',
  ).toBeNull();
});

test('leaving waking keeps the same player and opens the window', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  const onwatch = vi.fn();
  const { component } = render(TileHost, { onwatch });
  const player = document.querySelector('video-rtc');
  expect(player, 'the waking player').toBeTruthy();

  (component as unknown as { settle: () => void }).settle();
  await tick();
  expect(
    document.querySelector('video-rtc'),
    'the same element persists across waking -> live: the flip must not rebuild it',
  ).toBe(player);

  await vi.advanceTimersByTimeAsync(12000);
  await failVendoredPlayer();
  expect(
    screen.getByText(/did not start/),
    'and the window runs from the moment the state is live',
  ).toBeTruthy();
});

test('a tile starts muted with a speaker on it, and remembers the choice', async () => {
  vi.useFakeTimers();
  liveTile();
  const video = await videoOf();
  await vi.advanceTimersByTimeAsync(600);

  expect(video.muted, 'every tile starts muted').toBe(true);
  const speaker = screen.getByTestId('speaker');
  expect(speaker.getAttribute('aria-pressed'), 'the speaker starts off').toBe('false');
  expect(
    screen.queryByRole('button', { name: /^(Listen|Listening)$/ }),
    'no separate Listen button on a tile',
  ).toBeNull();

  speaker.click();
  await vi.advanceTimersByTimeAsync(600);

  expect(video.muted, 'the speaker unmutes this camera').toBe(false);
  expect(
    localStorage.getItem('nazar.audio.cam-1'),
    'and the choice is remembered per camera',
  ).toBe('1');
});

test('a live tile carries no still', () => {
  liveTile();

  expect(document.querySelector('canvas.still')).toBeNull();
});

test('a live tile waiting on its stream name says so', () => {
  const onwatch = vi.fn();
  render(CameraTile, {
    camera: { id: 'cam-1', name: 'Hall', state: 'live', reason: null },
    onwatch,
  });

  expect(
    screen.getByText(/Starting the stream/),
    'the window before the mint lands reads as starting, not as not streaming',
  ).toBeTruthy();
  expect(screen.queryByText(/Not streaming/), 'never as a dead camera').toBeNull();
});

async function videoOf(): Promise<HTMLVideoElement> {
  const video = document.querySelector('video-rtc video');
  expect(video, 'the player grew a video').toBeTruthy();
  return video as HTMLVideoElement;
}

test('a picture that never starts offers a retry', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  const { onwatch } = liveTile();

  expect(screen.queryByText(/did not start/), 'nothing is claimed too early').toBeNull();

  await vi.advanceTimersByTimeAsync(12000);
  await failVendoredPlayer();
  const note = screen.getByText(/did not start/);
  expect(note, 'no first frame inside the window is surfaced').toBeTruthy();

  screen.getByText('Try again').click();
  expect(onwatch, 'the retry asks for a fresh stream').toHaveBeenCalledWith('cam-1');
});

test('an advancing picture is left alone', async () => {
  vi.useFakeTimers();
  liveTile();
  const video = await videoOf();

  video.dispatchEvent(new Event('playing'));
  for (let tick = 1; tick <= 4; tick += 1) {
    video.currentTime = tick;
    await vi.advanceTimersByTimeAsync(5000);
  }

  expect(screen.queryByText(/stopped/), 'a moving picture raises nothing').toBeNull();
});

test('a second playing event does not double the watch', async () => {
  vi.useFakeTimers();
  liveTile();
  const video = await videoOf();
  const every = vi.spyOn(globalThis, 'setInterval');

  video.dispatchEvent(new Event('playing'));
  video.dispatchEvent(new Event('playing'));

  expect(every.mock.calls.length, 'one watch, not two').toBe(1);
  every.mockRestore();
});

test('a frozen picture asks for a retry', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  const { onwatch } = liveTile();
  const video = await videoOf();

  video.dispatchEvent(new Event('playing'));
  video.currentTime = 1;
  await vi.advanceTimersByTimeAsync(5000);
  expect(screen.queryByText(/stopped/), 'one advancing sample is not a freeze').toBeNull();

  await vi.advanceTimersByTimeAsync(10000);
  await failVendoredPlayer(/stopped/);
  expect(
    screen.getByText(/stopped/),
    'a stalled clock is surfaced once the fallback cannot take it',
  ).toBeTruthy();

  screen.getByText('Try again').click();
  expect(onwatch, 'the retry asks for a fresh stream').toHaveBeenCalledWith('cam-1');
});

test('a stalled picture is rebuilt on its own with a doubling backoff', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  const { onwatch } = liveTile();

  await vi.advanceTimersByTimeAsync(12000);
  await failVendoredPlayer();
  expect(screen.getByText(/did not start/), 'the stall is named').toBeTruthy();

  // Nobody clicks: the rebuild follows on the first backoff step.
  await vi.advanceTimersByTimeAsync(1000);
  expect(onwatch, 'the tile re-asks the server on its own').toHaveBeenCalledWith('cam-1');
  expect(
    screen.queryByText(/did not start/),
    'and the failure note clears for the fresh attempt',
  ).toBeNull();

  // The fresh attempt stalls too: the next rebuild waits the second step.
  await vi.advanceTimersByTimeAsync(12000);
  await failVendoredPlayer();
  onwatch.mockClear();
  await vi.advanceTimersByTimeAsync(1000);
  expect(
    onwatch,
    'the backoff doubled: nothing at the first step',
  ).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(1000);
  expect(onwatch, 'the second step fires at two seconds').toHaveBeenCalledWith('cam-1');
});

test('a fallback that takes over cancels the pending rebuild', async () => {
  vi.useFakeTimers();
  // This browser plays HLS by itself: the stall lands on the fallback.
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('maybe');
  const { onwatch } = liveTile();

  await vi.advanceTimersByTimeAsync(12500);
  expect(
    document.querySelector('[data-testid=hls]'),
    'the fallback took the picture',
  ).toBeTruthy();

  await vi.advanceTimersByTimeAsync(3000);
  expect(onwatch, 'a picture that settled is not rebuilt').not.toHaveBeenCalled();
  expect(screen.queryByText(/did not start/), 'and nothing is called a failure').toBeNull();
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

test('a backgrounded tab is never called a stalled picture', async () => {
  vi.useFakeTimers();
  const { onwatch } = liveTile();
  const video = await videoOf();
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 4;

  // Away: the browser throttles the page, the clock may not move, and
  // nothing may be concluded from a page that cannot see.
  setHidden(true);
  await vi.advanceTimersByTimeAsync(60000);
  expect(screen.queryByText(/stopped/), 'away is not a freeze').toBeNull();
  expect(document.querySelectorAll('video-rtc').length, 'and nothing rebuilt').toBe(1);
  expect(
    document.querySelector('script[src="/hls.min.js"]'),
    'no stall was even considered while the page could not see',
  ).toBeNull();

  // Back, with the picture still moving: the watch resumes where it was.
  video.currentTime = 64;
  setHidden(false);
  for (let step = 65; step <= 66; step += 1) {
    video.currentTime = step;
    await vi.advanceTimersByTimeAsync(5000);
  }
  expect(screen.queryByText(/stopped/), 'a moving picture raises nothing').toBeNull();
  expect(onwatch, 'and no restart was asked for').not.toHaveBeenCalled();
  expect(document.querySelectorAll('video-rtc').length, 'one player, still').toBe(1);
});

test('a picture that did not follow us back is started again', async () => {
  vi.useFakeTimers();
  const { onwatch } = liveTile();
  const video = await videoOf();
  const first = document.querySelector('video-rtc');
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 4;

  // Away, and the peer drops while we are not looking: the clock is exactly
  // where we left it when the tab comes back.
  setHidden(true);
  await vi.advanceTimersByTimeAsync(60000);
  setHidden(false);
  await vi.advanceTimersByTimeAsync(50);

  expect(
    document.querySelector('video-rtc'),
    'the dead player was replaced rather than left on the screen',
  ).not.toBe(first);
  expect(
    document.querySelectorAll('video-rtc').length,
    'one player at a time, the new one',
  ).toBe(1);
  expect(
    onwatch,
    'and the server is asked for the stream again: a long absence can outlive it',
  ).toHaveBeenCalledWith('cam-1');
  expect(screen.queryByText(/stopped/), 'the recovery is not framed as a failure').toBeNull();
});
