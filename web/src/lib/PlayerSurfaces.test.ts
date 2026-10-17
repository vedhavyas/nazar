//! The player surface revisions: audio always starts off with the native
//! control as the one unmute, a frozen picture holds its frame, and a
//! returning tab gets its beat before any rebuild.

// @vitest-environment jsdom

import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, beforeAll, expect, test, vi } from 'vitest';

import CameraTile from './CameraTile.svelte';

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
  for (const script of document.querySelectorAll('script[src="/hls.min.js"]')) {
    script.remove();
  }
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

test('a tile starts muted, has no speaker button, and the native control unmutes', async () => {
  vi.useFakeTimers();
  // A remembered ON from an older build must not bring back the sound.
  localStorage.setItem('nazar.audio.cam-1', '1');
  liveTile();
  await vi.advanceTimersByTimeAsync(600);

  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  expect(video.muted, 'every load starts muted').toBe(true);
  expect(screen.queryByTestId('speaker'), 'no separate speaker button').toBeNull();

  // The player's own control is the one unmute: its volumechange must not
  // be reverted by the mute mirror.
  video.muted = false;
  video.dispatchEvent(new Event('volumechange'));
  await vi.advanceTimersByTimeAsync(1200);
  expect(video.muted, 'the native control sticks').toBe(false);
});

test('a frozen picture holds its last frame and never stacks a fallback', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  const { onwatch } = liveTile();
  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 1;
  await vi.advanceTimersByTimeAsync(5000);
  await vi.advanceTimersByTimeAsync(10000);

  await vi.waitFor(() => {
    expect(screen.getByText(/stopped/), 'the stall is said quietly').toBeTruthy();
  });
  expect(
    document.querySelectorAll('script[src="/hls.min.js"]').length,
    'a painted picture never reaches for the fallback',
  ).toBe(0);
  expect(
    document.querySelector('[data-testid="player"]')?.classList.contains('gone'),
    'the last frame stays visible',
  ).toBe(false);

  const player = document.querySelector('video-rtc');
  await vi.advanceTimersByTimeAsync(2000);
  expect(onwatch, 'the server is re-asked on the ladder').toHaveBeenCalled();
  expect(
    document.querySelector('video-rtc'),
    'and the player is never remounted through a freeze',
  ).toBe(player);
  expect(
    video.currentTime,
    'the churn never zeroes the clock: the last frame stands where it was',
  ).toBe(1);

  // The heal: the clock moves again and the note clears in place.
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 2;
  await vi.advanceTimersByTimeAsync(1500);
  expect(screen.queryByText(/stopped/), 'the note clears when the picture returns').toBeNull();
  expect(document.querySelector('video-rtc'), 'still the same player').toBe(player);
});

test('a picture the element killed to zero says so instead of standing silent', async () => {
  vi.useFakeTimers();
  const { onwatch } = liveTile();
  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 3;
  await vi.advanceTimersByTimeAsync(5000);

  // The vendored element's own teardown (src cleared): the clock resets to
  // zero, playback pauses and the element stays empty. That pause is not a
  // person's - it is the silent death: Live on the label, 0:00 on the
  // element, and nothing on the screen.
  video.dispatchEvent(new Event('pause'));
  video.currentTime = 0;
  Object.defineProperty(video, 'readyState', { configurable: true, get: () => 0 });

  await vi.advanceTimersByTimeAsync(5500);
  await vi.waitFor(() => {
    expect(screen.getByText(/stopped/), 'the death is said').toBeTruthy();
  });
  expect(screen.getByText('Try again'), 'with the escape').toBeTruthy();
  await vi.advanceTimersByTimeAsync(2000);
  expect(onwatch, 'and the server is re-asked on the ladder').toHaveBeenCalled();
});

test('a tile whose picture never paints says so, whatever the player did', async () => {
  vi.useFakeTimers();
  // No events, no fallback resolution: the silent player the acceptance
  // photographed (Live, 0:00, blank, nothing said). The tile itself owns
  // the window now - no remount resets it.
  liveTile();
  await vi.advanceTimersByTimeAsync(21_000);
  expect(
    screen.getByText(/did not start/),
    'the tile says it, not the player',
  ).toBeTruthy();
  expect(screen.getByText('Try again'), 'with the escape').toBeTruthy();
});

test('stable props and no timers keep one player for good', async () => {
  vi.useFakeTimers();
  const camera = { id: 'cam-9', name: 'Hall', state: 'live' as const, reason: null };
  render(CameraTile, { camera, stream: 'cam-9-low', onwatch: vi.fn() });
  const first = document.querySelector('video-rtc');
  expect(first).toBeTruthy();
  await vi.advanceTimersByTimeAsync(20_000);
  expect(document.querySelector('video-rtc'), 'one player, no churn').toBe(first);
});

test('a person pausing a live picture is still not a stall', async () => {
  vi.useFakeTimers();
  liveTile();
  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 2;
  Object.defineProperty(video, 'readyState', { configurable: true, get: () => 4 });
  video.dispatchEvent(new Event('pause'));

  await vi.advanceTimersByTimeAsync(16000);
  expect(
    screen.queryByText(/stopped/),
    'a deliberate pause holds the frame and says nothing',
  ).toBeNull();
});

test('a fallback that never paints says so too', async () => {
  vi.useFakeTimers();
  vi.spyOn(HTMLMediaElement.prototype, 'canPlayType').mockReturnValue('');
  class StubHls {
    static isSupported = () => true;
    loadSource() {}
    attachMedia() {}
    destroy() {}
  }
  vi.stubGlobal('Hls', StubHls);
  liveTile();

  // The stage never negotiates: the start window fires and the fallback
  // takes the stage - and then the fallback itself sits at zero.
  await vi.advanceTimersByTimeAsync(12500);
  expect(document.querySelector('video.hls'), 'the fallback took the stage').toBeTruthy();

  // Its own window fires next; stop before the ladder's remount step so
  // this reads the note a person would be looking at.
  await vi.advanceTimersByTimeAsync(12000);
  expect(
    screen.getByText(/did not start/),
    'a fallback that never paints is said, not left standing',
  ).toBeTruthy();
  expect(screen.getByText('Try again'), 'with the escape').toBeTruthy();
});

test('a returning tab is given its beat before any rebuild', async () => {
  vi.useFakeTimers();
  const { onwatch } = liveTile();
  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  const player = document.querySelector('video-rtc');
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 4;

  setHidden(true);
  await vi.advanceTimersByTimeAsync(60000);
  setHidden(false);
  // The unthrottled tab resumes the clock within the grace.
  video.currentTime = 5;
  await vi.advanceTimersByTimeAsync(1500);

  expect(document.querySelector('video-rtc'), 'the same player, no flash').toBe(player);
  expect(onwatch, 'and no restart was asked for').not.toHaveBeenCalled();
});

test('a peer that really dropped still rebuilds after the grace', async () => {
  vi.useFakeTimers();
  const { onwatch } = liveTile();
  const video = document.querySelector('video-rtc video') as HTMLVideoElement;
  const first = document.querySelector('video-rtc');
  video.dispatchEvent(new Event('playing'));
  video.currentTime = 4;

  setHidden(true);
  await vi.advanceTimersByTimeAsync(60000);
  setHidden(false);
  // The clock stays put through the grace: a genuinely dead peer.
  await vi.advanceTimersByTimeAsync(6000);

  expect(
    document.querySelector('video-rtc'),
    'the dead player was replaced rather than left on the screen',
  ).not.toBe(first);
  expect(onwatch, 'and the server is asked for the stream again').toHaveBeenCalledWith('cam-1');
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
