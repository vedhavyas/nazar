//! The notification bridge: the unread count, the words an entry gets, the
//! deliberate no-still layout, and the telling - the shell first, the
//! browser when it is permitted.

// @vitest-environment jsdom

import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';

import type { Notification } from './api';
import { createNotifications, notify, since, type NotifySource } from './notifyBridge.svelte';
import NotifyBell from './NotifyBell.svelte';
import Notifications from '../routes/Notifications.svelte';

/** A source whose entries, names and arrivals the test drives. */
function fakeSource(entries: Notification[], names: Record<string, string> = {}) {
  let arrive: ((entry: Notification) => void) | null = null;
  const source: NotifySource = {
    list: vi.fn(async () => entries),
    names: vi.fn(async () => names),
    watch: vi.fn((onNew: (entry: Notification) => void) => {
      arrive = onNew;
      return () => {
        arrive = null;
      };
    }),
  };
  return { source, arrival: (entry: Notification) => arrive?.(entry) };
}

function entry(id: string, at: number, kind: Notification['kind']): Notification {
  const base = { id, device_id: 'cam-1', at, snapshot_url: null };
  return kind === 'aqi' ? { ...base, kind, value: 72 } : { ...base, kind };
}

/** The browser's notifications, in miniature. */
class FakeNotification {
  static permission: NotificationPermission = 'granted';
  static made: { title: string; body?: string }[] = [];
  constructor(title: string, options?: { body?: string }) {
    FakeNotification.made.push({ title, body: options?.body });
  }
}

beforeEach(() => {
  localStorage.clear();
  FakeNotification.made = [];
  FakeNotification.permission = 'granted';
  vi.stubGlobal('Notification', FakeNotification);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
  delete (window as { __TAURI__?: unknown }).__TAURI__;
});

test('the bell counts what arrived past the last look', async () => {
  const told: Notification[] = [];
  const fake = fakeSource([entry('b', 2000, 'offline'), entry('a', 1000, 'motion')]);
  const notes = createNotifications(fake.source, (arrival) => told.push(arrival));

  await notes.load();
  expect(notes.unread, 'both are unseen').toBe(2);

  notes.markSeen();
  expect(notes.unread, 'looking at the list is seeing them').toBe(0);

  // The look is remembered: a fresh visit does not resurrect the count.
  const reopened = createNotifications(fake.source, () => {});
  await reopened.load();
  expect(reopened.unread, 'what was seen stays seen').toBe(0);

  const stop = notes.follow();
  fake.arrival(entry('c', 3000, 'motion'));
  await vi.waitFor(() => {
    expect(notes.unread, 'an arrival past the look is unseen').toBe(1);
  });
  expect(notes.entries[0].id, 'newest first').toBe('c');
  expect(
    told.map((arrival) => arrival.id),
    'and the arrival was told out loud',
  ).toEqual(['c']);
  stop();
});

test('an arrival during the boot fetch survives it', async () => {
  let land: (() => void) | null = null;
  let raise: ((entry: Notification) => void) | null = null;
  const source: NotifySource = {
    list: () =>
      new Promise((resolve) => {
        land = () => resolve([entry('a', 1000, 'motion')]);
      }),
    names: async () => ({ 'cam-1': 'Hall' }),
    watch: (onNew) => {
      raise = onNew;
      return () => {
        raise = null;
      };
    },
  };
  const notes = createNotifications(source, () => {});
  const stop = notes.follow();

  const loading = notes.load();
  // The motion lands while the list is still in flight.
  raise?.(entry('c', 3000, 'motion'));
  land?.();
  await loading;

  expect(
    notes.entries.map((each) => each.id),
    'the arrival stays in front of the snapshot rather than being overwritten by it',
  ).toEqual(['c', 'a']);
  expect(notes.unread, 'and it counts').toBe(2);
  stop();
});

test('the bell wears its count and opens the list', () => {
  const onopen = vi.fn();
  render(NotifyBell, { unread: 3, onopen });
  expect(screen.getByTestId('unread').textContent, 'the count is shown').toBe('3');

  screen.getByRole('button', { name: /notifications/i }).click();
  expect(onopen, 'the bell is the door to the list').toHaveBeenCalled();

  cleanup();
  render(NotifyBell, { unread: 0, onopen });
  expect(screen.queryByTestId('unread'), 'nothing new wears no badge').toBeNull();
});

test("the list's relative times tick with the wall", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date('2026-10-16T12:00:00Z'));
  render(Notifications, {
    entries: [
      {
        id: 'a',
        device_id: 'cam-1',
        at: Date.now() - 120_000,
        snapshot_url: null,
        kind: 'motion',
      },
    ],
    names: { 'cam-1': 'Hall' },
    onback: vi.fn(),
    onopen: vi.fn(),
  });

  expect(screen.getByText('2m'), 'a two-minute-old entry reads as two minutes').toBeTruthy();
  await vi.advanceTimersByTimeAsync(60_000);
  expect(
    screen.getByText('3m'),
    'and the minute advances without a reload',
  ).toBeTruthy();
});

test('a still-less entry reads as a note, not a broken picture', () => {
  const onopen = vi.fn();
  render(Notifications, {
    entries: [
      { id: 'c', device_id: 'purifier', at: 6000, snapshot_url: null, kind: 'aqi', value: 72 },
      { id: 'b', device_id: 'cam-1', at: 5000, snapshot_url: null, kind: 'offline' },
      { id: 'a', device_id: 'cam-1', at: 1000, snapshot_url: '/api/snaps/a.jpg', kind: 'motion' },
    ],
    names: { 'cam-1': 'Hall' },
    onback: vi.fn(),
    onopen,
  });

  const stills = document.querySelectorAll('img');
  expect(stills.length, 'only the entry with a still carries a picture').toBe(1);
  expect(stills[0].getAttribute('src'), 'the still is the server’s own').toBe('/api/snaps/a.jpg');
  expect(screen.getByText('Offline'), 'the rest reads as a deliberate block').toBeTruthy();
  expect(screen.getByText('Motion at Hall'), 'a named camera is named').toBeTruthy();
  expect(screen.getByText('Air quality 72'), 'a device that is not a camera is not guessed').toBeTruthy();

  screen.getByText('Motion at Hall').closest('button')?.click();
  expect(onopen, 'a camera entry leads to that camera').toHaveBeenCalledWith('cam-1');
  expect(
    screen.getByText('Air quality 72').closest('button'),
    'a device with no screen has no door',
  ).toBeNull();

  expect(since(1000, 61_000), 'a minute reads as a minute').toBe('1m');
});

test('the shell is asked first, and falls back when the command is absent', async () => {
  // No shell at all: the browser carries it.
  expect(await notify('Motion at Hall', ''), 'no shell, the browser takes it').toBe('browser');
  expect(FakeNotification.made.length, 'and it was actually made').toBe(1);

  // A shell whose notify command does not exist yet (the state until M4/M5).
  const invoke = vi.fn(async () => {
    throw new Error('command notify not found');
  });
  (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = { invoke };
  FakeNotification.made = [];
  expect(
    await notify('Motion at Hall', ''),
    'an absent command falls back rather than dropping the telling',
  ).toBe('browser');
  expect(invoke, 'the shell was still asked first').toHaveBeenCalledWith('notify', {
    title: 'Motion at Hall',
    body: '',
  });
  expect(FakeNotification.made.length, 'and the browser caught it').toBe(1);

  // A shell that answers: the shell carries it, the browser stays quiet.
  invoke.mockImplementation(async () => null);
  FakeNotification.made = [];
  expect(await notify('Motion at Hall', ''), 'the shell takes it').toBe('shell');
  expect(FakeNotification.made.length, 'the browser stays out of the shell’s way').toBe(0);
  delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;

  // Nowhere permitted: nothing, said plainly.
  FakeNotification.permission = 'default';
  expect(await notify('Motion at Hall', ''), 'no shell, no permission: nothing').toBe('none');
});

test('every shape a shell may expose is a shell', async () => {
  // The other two globals a Tauri shell may carry, each on its own: the v2
  // global-injection shape, and v1's flat one. Trimming the probe chain to
  // any single shape must not leave the suite green.
  const shapes = [
    { shape: '__TAURI__.core.invoke', shell: { __TAURI__: { core: { invoke: vi.fn(async () => null) } } } },
    { shape: '__TAURI__.invoke', shell: { __TAURI__: { invoke: vi.fn(async () => null) } } },
  ];
  for (const { shape, shell } of shapes) {
    Object.assign(window, shell);
    FakeNotification.made = [];
    expect(await notify('Motion at Hall', ''), `the ${shape} shape is a shell`).toBe('shell');
    expect(FakeNotification.made.length, 'and the browser stays out of its way').toBe(0);
    delete (window as { __TAURI__?: unknown }).__TAURI__;
  }
});

test('an arrival is told through the bridge where it is permitted', async () => {
  const fake = fakeSource([], { 'cam-1': 'Hall' });
  const notes = createNotifications(fake.source);
  await notes.load();

  const stop = notes.follow();
  fake.arrival(entry('c', 3000, 'motion'));
  await vi.waitFor(() => {
    expect(FakeNotification.made[0]?.title, 'the arrival became a telling').toBe('Motion at Hall');
  });
  stop();
});
