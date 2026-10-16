//! The server API: one place for fetch, the error envelope, and the
//! server's own address. There is no cookie to carry: the session is a file
//! on the server, and the browser holds nothing.

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

/** True when the bundle runs inside a Tauri shell; false in a browser and
 * wherever there is no window at all. */
export function inShell(): boolean {
  if (typeof window === 'undefined') return false;
  const shell = window as { __TAURI_INTERNALS__?: unknown; __TAURI__?: unknown };
  return '__TAURI_INTERNALS__' in shell || '__TAURI__' in shell;
}

/** The server's address on the mesh, where a shell starts from. */
export const DEFAULT_SERVER = 'http://nazar.hub';

const STORED = 'nazar.server';

/** Trim, drop trailing slashes, and assume the mesh's scheme when none was
 * typed - a bare `nazar.hub` would otherwise read as one. */
export function normalizeAddress(address: string): string {
  const trimmed = address.trim().replace(/\/+$/, '');
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed) ? trimmed : `http://${trimmed}`;
}

/** Where requests point. Empty in a browser: the SPA is served by the server
 * itself, so paths stay relative. */
function resolveBase(): string {
  if (!inShell()) return '';
  const stored = localStorage.getItem(STORED);
  return stored ? normalizeAddress(stored) : DEFAULT_SERVER;
}

let base = resolveBase();

/** The current base; `''` means same-origin. */
export function serverBase(): string {
  return base;
}

/** Store an address and point everything at it from now on. */
export function setServerBase(address: string): void {
  base = normalizeAddress(address);
  localStorage.setItem(STORED, base);
}

/** The websocket origin the event sockets dial. */
export function socketBase(): string {
  return (base || location.origin).replace(/^http/, 'ws');
}

/** Whether a server answers at `address` - the setup screen's check, run
 * against a candidate before it is saved. */
export async function healthAt(address: string): Promise<{ ok: boolean; version: string }> {
  const response = await fetch(`${normalizeAddress(address)}/api/health`);
  if (!response.ok) {
    throw new ApiError(response.status, await errorMessage(response));
  }
  return (await response.json()) as { ok: boolean; version: string };
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

/** One account to choose between when a number maps to several. */
export interface ProfileChoice {
  uuid: string;
  name: string | null;
  email: string | null;
}

/** The server's hero decision: the camera that last saw the house move. */
export interface MotionHero {
  hero: string | null;
}

/** One recorded segment of a day: when a motion episode began, and the
 * still kept for it when one was. */
export interface Clip {
  id: string;
  start_ms: number;
  snapshot_url: string | null;
}

/** A told entry's shape on the wire; the air reading rides only on an aqi
 * entry. */
interface Told {
  id: string;
  device_id: string;
  /** Milliseconds since the epoch. */
  at: number;
  snapshot_url: string | null;
}

/** What the house found worth telling. */
export type Notification =
  | (Told & { kind: 'motion' | 'offline' | 'online' })
  | (Told & { kind: 'aqi'; value: number });

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
  const response = await fetch(`${base}${path}`, {
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
  // 204 signs straight in; a 200 answers the accounts to pick between.
  verifyOtp: (phone: string, code: string) =>
    call<{ profiles: ProfileChoice[] } | undefined>('/api/login/verify', {
      method: 'POST',
      body: JSON.stringify({ phone, code }),
    }),
  selectProfile: (uuid: string) => post('/api/login/select', { uuid }),
  cameras: () => call<Cameras>('/api/cameras'),
  // `retry=true`, spelled out: the server's query parser takes the words,
  // not `1`, and a wrong spelling is a 400 at the wire.
  watch: (id: string, quality: Quality = 'high', retry = false) =>
    call<{ stream: string }>(
      `/api/cameras/${id}/live?quality=${quality}${retry ? '&retry=true' : ''}`,
      { method: 'POST' },
    ),
  hero: () => call<MotionHero>('/api/motion/hero'),
  recordedDays: (id: string, from: number, to: number) =>
    call<{ days: string[] }>(`/api/cameras/${id}/recordings/days?from=${from}&to=${to}`),
  recordings: (id: string, day: string) =>
    call<{ clips: Clip[] }>(`/api/cameras/${id}/recordings?day=${encodeURIComponent(day)}`),
  playback: (id: string, timestamp: number) =>
    call<{ stream: string; expires_in: number }>(`/api/cameras/${id}/playback`, {
      method: 'POST',
      body: JSON.stringify({ timestamp }),
    }),
  playbackStop: (id: string) =>
    call<void>(`/api/cameras/${id}/playback`, { method: 'DELETE' }),
  notifications: () => call<{ notifications: Notification[] }>('/api/notifications'),
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
