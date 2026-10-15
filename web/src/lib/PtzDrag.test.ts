//! Dragging to pan: the camera's own signs, and no more than four writes a
//! second however fast the finger moves.

// @vitest-environment jsdom

import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, expect, test, vi } from 'vitest';

import PtzDrag from './PtzDrag.svelte';

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

function pad() {
  const onstep = vi.fn();
  render(PtzDrag, { onstep });
  const surface = screen.getByTestId('ptz-surface');
  return { onstep, surface };
}

/** One whole drag: down, a move of (dx, dy), and the release. */
function drag(surface: HTMLElement, dx: number, dy: number) {
  surface.dispatchEvent(
    new MouseEvent('pointerdown', { clientX: 200, clientY: 200, bubbles: true }),
  );
  surface.dispatchEvent(
    new MouseEvent('pointermove', { clientX: 200 + dx, clientY: 200 + dy, bubbles: true }),
  );
}

test('a left drag pans left, in the camera’s signs', async () => {
  vi.useFakeTimers();
  const { onstep, surface } = pad();

  drag(surface, -100, 0);
  await vi.advanceTimersByTimeAsync(300);

  expect(onstep, 'one step went out').toHaveBeenCalledTimes(1);
  expect(onstep.mock.calls[0][0], 'a leftward drag is a positive x step').toBe(40);
  expect(onstep.mock.calls[0][1], 'and it stays level').toBe(0);
});

test('an upward drag tips the camera up', async () => {
  vi.useFakeTimers();
  const { onstep, surface } = pad();

  drag(surface, 0, -100);
  await vi.advanceTimersByTimeAsync(300);

  expect(onstep.mock.calls[0][0], 'nothing horizontal').toBe(0);
  expect(onstep.mock.calls[0][1], 'an upward drag is a positive y step').toBe(40);
});

test('five moves inside one window are one write', async () => {
  vi.useFakeTimers();
  const { onstep, surface } = pad();

  surface.dispatchEvent(
    new MouseEvent('pointerdown', { clientX: 0, clientY: 0, bubbles: true }),
  );
  for (let step = 1; step <= 5; step += 1) {
    surface.dispatchEvent(
      new MouseEvent('pointermove', { clientX: step * 20, clientY: 0, bubbles: true }),
    );
  }
  await vi.advanceTimersByTimeAsync(300);

  expect(
    onstep.mock.calls.length,
    'a fast drag is one write in the window, not one per move',
  ).toBe(1);
});

test('a slow drag writes once per window', async () => {
  vi.useFakeTimers();
  const { onstep, surface } = pad();

  surface.dispatchEvent(
    new MouseEvent('pointerdown', { clientX: 0, clientY: 0, bubbles: true }),
  );
  for (let step = 1; step <= 4; step += 1) {
    surface.dispatchEvent(
      new MouseEvent('pointermove', { clientX: step * 60, clientY: 0, bubbles: true }),
    );
    await vi.advanceTimersByTimeAsync(300);
  }

  expect(onstep.mock.calls.length, 'four windows, four writes').toBe(4);
});

test('a drag that ends sends nothing more', async () => {
  vi.useFakeTimers();
  const { onstep, surface } = pad();

  drag(surface, -100, 0);
  await vi.advanceTimersByTimeAsync(300);
  expect(onstep).toHaveBeenCalledTimes(1);

  surface.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
  surface.dispatchEvent(
    new MouseEvent('pointermove', { clientX: 500, clientY: 500, bubbles: true }),
  );
  await vi.advanceTimersByTimeAsync(600);

  expect(onstep, 'the release ends the drag').toHaveBeenCalledTimes(1);
});

test('the direction shows while the finger is down', async () => {
  vi.useFakeTimers();
  const { surface } = pad();

  expect(screen.queryByTestId('ptz-arrow'), 'nothing is shown until a drag').toBeNull();

  drag(surface, -100, 0);
  expect(await screen.findByTestId('ptz-arrow'), 'the direction is shown').toBeTruthy();

  surface.dispatchEvent(new MouseEvent('pointerup', { bubbles: true }));
  await vi.waitFor(() => {
    expect(screen.queryByTestId('ptz-arrow'), 'and cleared on release').toBeNull();
  });
});
