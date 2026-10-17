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
