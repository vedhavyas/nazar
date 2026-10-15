//! The tile's liveness watch: the no-first-frame stall, the frozen picture,
//! and the retry both offer.

// @vitest-environment jsdom

import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, beforeAll, expect, test, vi } from 'vitest';

import CameraTile from './CameraTile.svelte';

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

test('without native HLS the tile offers the retry instead', async () => {
  vi.useFakeTimers();
  // Desktop Chromium and Firefox do not play HLS themselves.
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  liveTile();

  await vi.advanceTimersByTimeAsync(12_500);

  expect(screen.getByText(/did not start/), 'the person gets the retry').toBeTruthy();
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

test('a live tile carries no still', () => {
  liveTile();

  expect(document.querySelector('canvas.still')).toBeNull();
});

async function videoOf(): Promise<HTMLVideoElement> {
  const video = document.querySelector('video-rtc video');
  expect(video, 'the player grew a video').toBeTruthy();
  return video as HTMLVideoElement;
}

test('a picture that never starts offers a retry', async () => {
  vi.useFakeTimers();
  const { onwatch } = liveTile();

  expect(screen.queryByText(/did not start/), 'nothing is claimed too early').toBeNull();

  await vi.advanceTimersByTimeAsync(12000);
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
  const { onwatch } = liveTile();
  const video = await videoOf();

  video.dispatchEvent(new Event('playing'));
  video.currentTime = 1;
  await vi.advanceTimersByTimeAsync(5000);
  expect(screen.queryByText(/stopped/), 'one advancing sample is not a freeze').toBeNull();

  await vi.advanceTimersByTimeAsync(10000);
  expect(screen.getByText(/stopped/), 'a stalled clock is surfaced').toBeTruthy();

  screen.getByText('Try again').click();
  expect(onwatch, 'the retry asks for a fresh stream').toHaveBeenCalledWith('cam-1');
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

  expect(onwatch, 'the tile asks for the stream again').toHaveBeenCalledWith('cam-1');
  expect(
    document.querySelector('video-rtc'),
    'and the dead player was replaced rather than left on the screen',
  ).not.toBe(first);
  expect(
    document.querySelectorAll('video-rtc').length,
    'one player at a time, the new one',
  ).toBe(1);
  expect(screen.queryByText(/stopped/), 'the recovery is not framed as a failure').toBeNull();
});
