//! The talk button: shells only, honest about being unavailable, and a hold
//! that always ends - including when the lane drops mid-press.

// @vitest-environment jsdom

import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, expect, test, vi } from 'vitest';

import TalkButton from './TalkButton.svelte';

afterEach(() => {
  cleanup();
  delete (window as unknown as Record<string, unknown>).__TAURI__;
});

function enterShell() {
  (window as unknown as Record<string, unknown>).__TAURI__ = {};
}

function hold(button: HTMLElement, type: 'pointerdown' | 'pointerup') {
  button.dispatchEvent(new MouseEvent(type, { bubbles: true }));
}

test('the browser build shows no talk button', () => {
  render(TalkButton, { armed: true, onstart: vi.fn(), onstop: vi.fn() });
  expect(
    screen.queryByTestId('talk-button'),
    'the web build has no microphone path, so there is nothing to show',
  ).toBeNull();
});

test('an unarmed lane disables the button with the reason', () => {
  enterShell();
  render(TalkButton, {
    armed: false,
    reason: 'the fast lane is not armed',
    onstart: vi.fn(),
    onstop: vi.fn(),
  });

  const button = screen.getByTestId('talk-button') as HTMLButtonElement;
  expect(button.disabled, 'an unarmed lane is never silently ready').toBe(true);
  expect(
    screen.getByText('the fast lane is not armed'),
    'the reason is on the button, not in a tooltip only',
  ).toBeTruthy();
});

test('a hold reports start once and the release reports stop', async () => {
  enterShell();
  const onstart = vi.fn();
  const onstop = vi.fn();
  render(TalkButton, { armed: true, onstart, onstop });

  const button = screen.getByTestId('talk-button') as HTMLButtonElement;
  expect(button.disabled, 'an armed lane is pressable').toBe(false);

  hold(button, 'pointerdown');
  expect(onstart, 'the press starts the burst').toHaveBeenCalledTimes(1);
  hold(button, 'pointerdown');
  expect(onstart, 'a second press while held changes nothing').toHaveBeenCalledTimes(1);

  hold(button, 'pointerup');
  expect(onstop, 'the release ends the burst').toHaveBeenCalledTimes(1);
  hold(button, 'pointerup');
  expect(onstop, 'a second release changes nothing').toHaveBeenCalledTimes(1);
});

test('a lane that drops mid-hold ends the burst', async () => {
  enterShell();
  const onstart = vi.fn();
  const onstop = vi.fn();
  const { rerender } = render(TalkButton, { armed: true, onstart, onstop });

  const button = screen.getByTestId('talk-button') as HTMLButtonElement;
  hold(button, 'pointerdown');
  expect(onstart).toHaveBeenCalledTimes(1);

  await rerender({ armed: false, onstart, onstop });
  expect(
    onstop,
    'a dropped lane releases the talk rather than swallowing it',
  ).toHaveBeenCalledTimes(1);
  expect(button.disabled, 'and the button turns disabled').toBe(true);
});
