//! The tile-level picture watch: a picture that never paints is said once
//! per absence, whatever the player component is doing (or being remounted
//! into).

// @vitest-environment jsdom

import { afterEach, expect, test, vi } from 'vitest';

import { pictureWatch } from './liveness';

function host(video: HTMLVideoElement | null = null) {
  const node = document.createElement('div');
  if (video) node.appendChild(video);
  document.body.appendChild(node);
  return node;
}

function setPicture(video: HTMLVideoElement, holds: boolean) {
  Object.defineProperty(video, 'readyState', { configurable: true, get: () => (holds ? 4 : 0) });
  video.currentTime = holds ? 2 : 0;
}

afterEach(() => {
  vi.useRealTimers();
  document.body.innerHTML = '';
});

test('a picture that never paints is announced once the window is out', () => {
  vi.useFakeTimers();
  const node = host(document.createElement('video'));
  const verdicts: string[] = [];
  const watch = pictureWatch(() => node, (v) => verdicts.push(v));

  vi.advanceTimersByTime(19_000);
  expect(verdicts, 'inside the window nothing is said').toEqual([]);
  vi.advanceTimersByTime(4_000);
  expect(verdicts, 'the window is out: said once').toEqual(['start']);
  vi.advanceTimersByTime(30_000);
  expect(verdicts, 'and not repeated while it stays missing').toEqual(['start']);
  watch.cancel();
});

test('a picture that paints clears the word; a second absence speaks again', () => {
  vi.useFakeTimers();
  const video = document.createElement('video');
  const node = host(video);
  const verdicts: string[] = [];
  const watch = pictureWatch(() => node, (v) => verdicts.push(v));

  vi.advanceTimersByTime(21_000);
  expect(verdicts).toEqual(['start']);

  setPicture(video, true);
  vi.advanceTimersByTime(2_500);
  expect(verdicts, 'the picture arrived: the word clears').toEqual(['start', 'painted']);

  // It dies again and stays dead: the tile may not fall back to silence.
  setPicture(video, false);
  vi.advanceTimersByTime(24_000);
  expect(verdicts, 'a fresh absence is said again').toEqual(['start', 'painted', 'start']);
  watch.cancel();
});

test('a blocked window does not run (the server is still waking)', () => {
  vi.useFakeTimers();
  const node = host(document.createElement('video'));
  let waking = true;
  const verdicts: string[] = [];
  const watch = pictureWatch(
    () => node,
    (v) => verdicts.push(v),
    () => waking,
  );

  vi.advanceTimersByTime(60_000);
  expect(verdicts, 'a waking stream is not a dead one').toEqual([]);
  waking = false;
  vi.advanceTimersByTime(24_000);
  expect(verdicts, 'once awake, the window runs').toEqual(['start']);
  watch.cancel();
});

test('a picture already painted never gets the word later', () => {
  vi.useFakeTimers();
  const video = document.createElement('video');
  setPicture(video, true);
  const node = host(video);
  const verdicts: string[] = [];
  const watch = pictureWatch(() => node, (v) => verdicts.push(v));

  vi.advanceTimersByTime(30_000);
  expect(verdicts, 'a moved clock is a picture').toEqual(['painted']);
  watch.cancel();
});
