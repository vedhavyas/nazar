//! The server API: one place for fetch, the error envelope, and the
//! session cookie (HttpOnly, so `same-origin` credentials is what makes it
//! travel).

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

async function call<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(path, {
    credentials: 'same-origin',
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
  watch: (id: string, quality: Quality = 'high') =>
    call<{ stream: string }>(`/api/cameras/${id}/live?quality=${quality}`, { method: 'POST' }),
  hero: () => call<MotionHero>('/api/motion/hero'),
};
