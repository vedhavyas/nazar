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
});

function liveTile() {
  const onwatch = vi.fn();
  render(CameraTile, {
    camera: { id: 'cam-1', name: 'Hall', state: 'live', reason: null },
    onwatch,
  });
  return { onwatch };
}

test('away mode shows a still over the tile it already streams', async () => {
  const onwatch = vi.fn();
  render(CameraTile, {
    camera: { id: 'cam-1', name: 'Hall', state: 'live', reason: null },
    onwatch,
    still: true,
  });

  expect(
    document.querySelector('canvas.still'),
    'the tile shows a still rather than the live picture',
  ).toBeTruthy();
  expect(
    document.querySelector('.stage.hidden'),
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
