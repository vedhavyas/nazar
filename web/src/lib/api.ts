//! The server API: one place for fetch and the error envelope. There is no
//! cookie to carry: the session is a file on the server, and the browser
//! holds nothing.

/** What a failed call throws; `message` is the server's own words. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

export interface Camera {
  id: string;
  name: string;
  state: 'offline' | 'waking' | 'live' | 'degraded';
  reason: string | null;
}

/** The camera page: the list, and whether Qubo has refused the session. */
export interface Cameras {
  cameras: Camera[];
  needsLogin: boolean;
}

/** The two stream classes the gateway mints. */
export type Quality = 'high' | 'low';

/** The server's hero decision: the camera that last saw the house move. */
export interface MotionHero {
  hero: string | null;
}

/** A move's outcome: the camera's own shadow, never its ack. */
export interface PtzResult {
  ok: boolean;
  reason?: string;
  position: { x: number; y: number } | null;
}

/** A privacy write's outcome, read back from the shadow. */
export interface PrivacyResult {
  ok: boolean;
  reason?: string;
  privacy: boolean | null;
}

async function call<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: init.body ? { 'content-type': 'application/json' } : undefined,
  });
  if (!response.ok) {
    throw new ApiError(response.status, await errorMessage(response));
  }
  if (response.status === 204) {
    return undefined as T;
  }
  return (await response.json()) as T;
}

async function errorMessage(response: Response): Promise<string> {
  try {
    const body = (await response.json()) as { error?: unknown };
    if (typeof body.error === 'string' && body.error) {
      return body.error;
    }
  } catch {
    // Not our envelope; the status line below is the most honest reason.
  }
  return `${response.status} ${response.statusText}`;
}

function post(path: string, body: unknown): Promise<void> {
  return call<void>(path, { method: 'POST', body: JSON.stringify(body) });
}

export const api = {
  health: () => call<{ ok: boolean; version: string }>('/api/health'),
  requestOtp: (phone: string) => post('/api/login/otp', { phone }),
  verifyOtp: (phone: string, code: string) => post('/api/login/verify', { phone, code }),
  cameras: () => call<Cameras>('/api/cameras'),
  // `retry=true`, spelled out: the server's query parser takes the words,
  // not `1`, and a wrong spelling is a 400 at the wire.
  watch: (id: string, quality: Quality = 'high', retry = false) =>
    call<{ stream: string }>(
      `/api/cameras/${id}/live?quality=${quality}${retry ? '&retry=true' : ''}`,
      { method: 'POST' },
    ),
  hero: () => call<MotionHero>('/api/motion/hero'),
  ptz: (id: string, dx: number, dy: number) =>
    call<PtzResult>(`/api/cameras/${id}/ptz`, { method: 'POST', body: JSON.stringify({ dx, dy }) }),
  privacy: (id: string, on: boolean) =>
    call<PrivacyResult>(`/api/cameras/${id}/privacy`, {
      method: 'POST',
      body: JSON.stringify({ on }),
    }),
  privacyState: (id: string) =>
    call<{ privacy: boolean | null }>(`/api/cameras/${id}/privacy`),
};
