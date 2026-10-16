//! The setup screen: the address field, the check that must answer before
//! it saves, and the failure shown in the check's own words.

// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, expect, test, vi } from 'vitest';

import Setup from './Setup.svelte';

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  localStorage.clear();
});

test("an address that does not answer is shown in the check's own words", async () => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => {
      throw new Error('Failed to fetch');
    }),
  );

  render(Setup, { props: { onready: () => {} } });

  const failure = await screen.findByRole('alert');
  expect(failure.textContent, 'the reason, verbatim').toBe('Failed to fetch');
});

test('an answering address is saved and handed on', async () => {
  const fetchMock = vi.fn(async (url: string) => {
    if (url === 'http://house.lan:9000/api/health') {
      return new Response(JSON.stringify({ ok: true, version: '0.3.0' }), { status: 200 });
    }
    throw new Error('Failed to fetch');
  });
  vi.stubGlobal('fetch', fetchMock);
  const ready = vi.fn();

  render(Setup, { props: { onready: ready } });

  // The stored default is tried on the way in, fails, and its reason shows.
  await screen.findByRole('alert');
  await fireEvent.input(screen.getByLabelText('Server address'), {
    target: { value: 'http://house.lan:9000' },
  });
  await fireEvent.click(screen.getByRole('button', { name: 'Connect' }));

  await vi.waitFor(() => expect(ready, 'the app is handed back once saved').toHaveBeenCalled());
  expect(localStorage.getItem('nazar.server'), 'the address outlives the window').toBe(
    'http://house.lan:9000',
  );
  expect(screen.queryByRole('alert'), 'a saved address leaves no failure behind').toBeNull();
});
