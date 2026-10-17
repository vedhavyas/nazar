//! The app's boot: a transient refusal must not park a tab on Login.

// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, expect, test, vi } from 'vitest';

import App from './App.svelte';

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function json(body: unknown): Response {
  return new Response(JSON.stringify(body), { status: 200 });
}

/** Hide or show the page, as a browser tab switch does. */
function setHidden(hidden: boolean) {
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden });
  Object.defineProperty(document, 'visibilityState', {
    configurable: true,
    get: () => (hidden ? 'hidden' : 'visible'),
  });
  document.dispatchEvent(new Event('visibilitychange'));
}

test('a login parked by a transient refusal returns home on coming back', async () => {
  let refuse = true;
  vi.stubGlobal(
    'fetch',
    vi.fn(async (url: string) => {
      const path = String(url);
      if (path.startsWith('/api/cameras')) {
        if (refuse) throw new Error('the server is away');
        return json({ cameras: [], needsLogin: false });
      }
      if (path.startsWith('/api/health')) return json({ ok: true, version: 'test' });
      if (path.startsWith('/api/notifications')) return json({ entries: [], names: {} });
      return json({});
    }),
  );

  render(App);
  // The refusal parks the tab on Login...
  await waitFor(() =>
    expect(screen.getByText('Sign in'), 'the refusal lands on Login').toBeTruthy(),
  );

  // ...but coming back to the tab re-checks, and a clean answer goes home.
  refuse = false;
  setHidden(true);
  setHidden(false);

  await waitFor(() =>
    expect(screen.getByText('nazar'), 'the tab is home again').toBeTruthy(),
  );
});
