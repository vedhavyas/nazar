//! Following the server's hero: the big tile is whichever camera the
//! server says moved, and away mode turns the small ones into stills.

// @vitest-environment jsdom

import { expect, test, vi } from 'vitest';

import type { Camera } from './api';
import { createHero, serverSource, type HeroSource } from './useHero.svelte';

function camera(id: string, name: string): Camera {
  return { id, name, state: 'live', reason: null };
}

/** A source whose hero answer and event callbacks the test drives. */
function fakeSource(hero: string | null) {
  let answer = hero;
  let raise: (() => void) | null = null;
  const source: HeroSource = {
    hero: vi.fn(async () => answer),
    watch: vi.fn((onEvent: () => void) => {
      raise = onEvent;
      return () => {
        raise = null;
      };
    }),
  };
  return {
    source,
    set: (next: string | null) => {
      answer = next;
    },
    event: () => raise?.(),
  };
}

const hall = camera('hall', 'Hall');
const gate = camera('gate', 'Gate');
const deck = camera('deck', 'Deck');

test("the server's hero takes the big tile", async () => {
  const fake = fakeSource('gate');
  const house = createHero(() => [hall, gate, deck], fake.source);

  await house.refresh();

  expect(
    house.tiles.map((tile) => tile.camera.id),
    'the hero leads, the rest keep their order',
  ).toEqual(['gate', 'hall', 'deck']);
  expect(
    house.tiles.map((tile) => tile.hero),
    'one tile is the hero',
  ).toEqual([true, false, false]);
});

test('a hero nobody has changes nothing', async () => {
  const fake = fakeSource('gone');
  const house = createHero(() => [hall, gate], fake.source);

  await house.refresh();

  expect(
    house.tiles.map((tile) => tile.camera.id),
    'the server is not second-guessed with a camera the account lost',
  ).toEqual(['hall', 'gate']);
  expect(
    house.tiles.some((tile) => tile.hero),
    'and no tile is claimed as the hero',
  ).toBe(false);
});

test('an empty house has no hero', async () => {
  const fake = fakeSource(null);
  const house = createHero(() => [], fake.source);

  await house.refresh();

  expect(house.tiles).toEqual([]);
  expect(house.hero).toBeNull();
});

test('away mode stills the small tiles and never the hero', async () => {
  const fake = fakeSource('gate');
  const house = createHero(() => [hall, gate], fake.source);
  await house.refresh();

  house.setAway(true);

  expect(
    house.tiles.map((tile) => [tile.camera.id, tile.still]),
    'the hero stays live; the tiles go still',
  ).toEqual([
    ['gate', false],
    ['hall', true],
  ]);

  house.setAway(false);

  expect(
    house.tiles.map((tile) => tile.still),
    'and the stills go back to live',
  ).toEqual([false, false]);
});

test('the event socket does not come back after the teardown', async () => {
  vi.useFakeTimers();
  let made = 0;
  class FakeSocket {
    onmessage: (() => void) | null = null;
    onclose: (() => void) | null = null;
    constructor() {
      made += 1;
    }
    close() {
      this.onclose?.();
    }
  }
  vi.stubGlobal('WebSocket', FakeSocket);

  const source = serverSource();
  const stop = source.watch(() => {});
  expect(made, 'the socket opens').toBe(1);

  stop();
  // Past the reconnect delay, not merely past a tick: the re-arm this
  // guards against is three seconds out.
  await vi.advanceTimersByTimeAsync(5_000);

  expect(made, 'a closed socket must not re-arm after teardown').toBe(1);
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

test('an event re-asks the server who is big', async () => {
  const fake = fakeSource('hall');
  const house = createHero(() => [hall, gate], fake.source);
  await house.refresh();
  expect(house.tiles[0].camera.id, 'the first answer is followed').toBe('hall');

  const stop = house.follow();
  fake.set('gate');
  fake.event();

  await vi.waitFor(() => {
    expect(
      house.tiles.map((tile) => tile.camera.id),
      'the event brought a new hero to the big tile',
    ).toEqual(['gate', 'hall']);
  });

  stop();
});
