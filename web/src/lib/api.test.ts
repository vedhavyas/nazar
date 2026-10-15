//! The API client: the error envelope, cookie carriage, and call shapes.

import { afterEach, expect, test, vi } from 'vitest';

import { ApiError, api } from './api';

afterEach(() => {
  vi.unstubAllGlobals();
});

function stubFetch(status: number, body: string) {
  // 204 carries no body, and `Response` enforces that.
  const fetchMock = vi.fn(async () => new Response(status === 204 ? null : body, { status }));
  vi.stubGlobal('fetch', fetchMock);
  return fetchMock;
}

test('a successful call answers the parsed body', async () => {
  stubFetch(200, JSON.stringify({ ok: true, version: '1.2.3' }));

  const health = await api.health();

  expect(health, 'the body is parsed').toEqual({ ok: true, version: '1.2.3' });
});

test("a failed call carries the server's error verbatim", async () => {
  stubFetch(401, JSON.stringify({ error: 'wrong PIN' }));

  const failure = await api.unlock('0000').then(
    () => null,
    (error: unknown) => error,
  );

  expect(failure, 'the call rejects').toBeInstanceOf(ApiError);
  expect((failure as ApiError).status, 'the status is kept').toBe(401);
  expect((failure as ApiError).message, 'the reason is verbatim').toBe('wrong PIN');
});

test('a failed call without the envelope falls back to the status line', async () => {
  stubFetch(502, '<html>bad gateway</html>');

  const failure = (await api.cameras().then(
    () => null,
    (error: unknown) => error,
  )) as ApiError;

  expect(failure.message, 'a non-envelope body still yields a reason').toContain('502');
});

test('every call asks for same-origin credentials and the documented shape', async () => {
  const fetchMock = stubFetch(204, '');

  await api.unlock('2468');

  const [path, init] = fetchMock.mock.calls[0];
  expect(path, 'the route').toBe('/api/unlock');
  expect(init?.method, 'the method').toBe('POST');
  expect(init?.credentials, 'the HttpOnly cookie rides along').toBe('same-origin');
  expect(init?.body, 'the body').toBe(JSON.stringify({ pin: '2468' }));
});

test('watching a camera calls its live route', async () => {
  const fetchMock = stubFetch(200, JSON.stringify({ stream: 'cam-1' }));

  const watch = await api.watch('cam-1');

  expect(watch.stream, 'the stream name comes back').toBe('cam-1');
  expect(fetchMock.mock.calls[0][0], 'the route').toBe('/api/cameras/cam-1/live');
});
