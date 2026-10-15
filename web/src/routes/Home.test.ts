//! Home's routing half of the needs-login signal: a refused Qubo session
//! must land the browser back on login.

// @vitest-environment jsdom

import { cleanup, render, waitFor } from '@testing-library/svelte';
import { afterEach, expect, test, vi } from 'vitest';

import Home from './Home.svelte';

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

test('a refused session routes Home back to login', async () => {
  vi.stubGlobal(
    'fetch',
    vi.fn(
      async () =>
        new Response(JSON.stringify({ cameras: [], needsLogin: true }), { status: 200 }),
    ),
  );
  const onlocked = vi.fn();

  render(Home, { onlocked });

  await waitFor(() => expect(onlocked, 'Home hands the browser back to login').toHaveBeenCalled());
});
