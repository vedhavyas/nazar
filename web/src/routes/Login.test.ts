//! The login screen: the code step, the profile pick when one number holds
//! two accounts, and the server's own words on a refusal.

// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, expect, test, vi } from 'vitest';

import Login from './Login.svelte';

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  localStorage.clear();
});

const ACCOUNTS = [
  {
    uuid: '10000000-0000-4000-8000-000000000001',
    name: 'Vedhavyas',
    email: 'vedhavyas@example.com',
  },
  {
    uuid: '10000000-0000-4000-8000-000000000003',
    name: 'Vedhavyas (work)',
    email: 'work@example.com',
  },
];

/** Route stub: every route answers its Response, fresh per call. */
function stubLogin(routes: Record<string, Response>) {
  const fetchMock = vi.fn(async (url: string, _init?: RequestInit) => {
    const route = routes[url];
    if (!route) throw new Error(`no stub for ${url}`);
    return route.clone();
  });
  vi.stubGlobal('fetch', fetchMock);
  return fetchMock;
}

function noContent() {
  return new Response(null, { status: 204 });
}

function jsonBody(status: number, body: unknown) {
  return new Response(JSON.stringify(body), { status });
}

/** Type the number, ask for a code, and hand the code in. */
async function signIn() {
  await fireEvent.input(screen.getByLabelText('Phone number'), {
    target: { value: '9876543210' },
  });
  await fireEvent.click(screen.getByRole('button', { name: 'Send code' }));
  await fireEvent.input(await screen.findByLabelText('Code'), {
    target: { value: '123456' },
  });
  await fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));
}

test('a number on two accounts gets a picker, and the pick signs in', async () => {
  const fetchMock = stubLogin({
    '/api/login/otp': noContent(),
    '/api/login/verify': jsonBody(200, { profiles: ACCOUNTS }),
    '/api/login/select': noContent(),
  });
  const loggedin = vi.fn();

  render(Login, { props: { onloggedin: loggedin } });
  await signIn();

  const work = await screen.findByRole('button', { name: /work@example\.com/ });
  expect(
    screen.getByRole('button', { name: /vedhavyas@example\.com/ }),
    'both accounts are offered',
  ).toBeTruthy();
  expect(loggedin, 'nothing signs in until a pick').not.toHaveBeenCalled();

  await fireEvent.click(work);

  await vi.waitFor(() => expect(loggedin, 'the pick signs in').toHaveBeenCalled());
  const select = fetchMock.mock.calls.find(([url]) => url === '/api/login/select');
  expect(select, 'the pick reaches the select route').toBeTruthy();
  expect(
    JSON.parse(String((select![1] as RequestInit).body)),
    'the picked uuid rides the body',
  ).toEqual({ uuid: ACCOUNTS[1].uuid });
});

test('a single profile signs in without a pick', async () => {
  stubLogin({
    '/api/login/otp': noContent(),
    '/api/login/verify': noContent(),
  });
  const loggedin = vi.fn();

  render(Login, { props: { onloggedin: loggedin } });
  await signIn();

  await vi.waitFor(() => expect(loggedin, 'the code signs in').toHaveBeenCalled());
  expect(screen.queryByText(/more than one/), 'no picker step appears').toBeNull();
});

test("a refused pick shows the server's words", async () => {
  stubLogin({
    '/api/login/otp': noContent(),
    '/api/login/verify': jsonBody(200, { profiles: ACCOUNTS }),
    '/api/login/select': jsonBody(401, { error: 'no account profile with that id' }),
  });
  const loggedin = vi.fn();

  render(Login, { props: { onloggedin: loggedin } });
  await signIn();
  await fireEvent.click(await screen.findByRole('button', { name: /work@example\.com/ }));

  const failure = await screen.findByRole('alert');
  expect(failure.textContent, 'the reason, verbatim').toBe('no account profile with that id');
  expect(loggedin, 'a refused pick does not sign in').not.toHaveBeenCalled();
});
