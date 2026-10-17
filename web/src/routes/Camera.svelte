<script lang="ts">
  import { onMount } from 'svelte';

  import { ApiError, api, type Camera as CameraInfo, type Quality } from '../lib/api';
  import { pictureWatch } from '../lib/liveness';
  import Player from '../lib/Player.svelte';
  import PtzDrag from '../lib/PtzDrag.svelte';
  import { rebuildSchedule } from '../lib/rebuild';

  let {
    id,
    onback,
    onlocked,
    onrecordings,
  }: {
    id: string;
    onback: () => void;
    onlocked: () => void;
    onrecordings?: () => void;
  } = $props();

  let camera = $state<CameraInfo | null>(null);
  let quality = $state<Quality>('high');
  let stream = $state<string | null>(null);
  let error = $state('');
  /** The lens state the camera reports, never the ack. `null` is unknown. */
  let privacy = $state<boolean | null>(null);
  /** What the last move did, from the shadow. */
  let move = $state('');

  let stage = $state<HTMLDivElement | null>(null);
  let attempt = $state(0);
  /** The server is still waking this stream: the player's start window is
   * held until its own word says otherwise. */
  let waiting = $state(false);
  /** The picture is not moving: 'start' never negotiated, 'frozen' stopped. */
  let stalled = $state<null | 'start' | 'frozen'>(null);
  /** Whether the fallback stream is carrying the picture. */
  let fallingBack = $state(false);

  // A stalled picture heals itself on a bounded backoff: a never-started
  // one re-asks and remounts, a frozen one only re-asks - its frame is
  // held, and the quiet note stays until the picture returns.
  const rebuild = rebuildSchedule((kind) => {
    if (kind === 'frozen') {
      void recover();
      return;
    }
    // The note stays through the rebuild: it is the truth until a picture
    // actually arrives, and the re-ask may not conjure one.
    waiting = false;
    void start().catch(() => {
      // Best-effort, like recover(): the stall itself is what shows.
    });
  });
  $effect(() => () => rebuild.cancel());

  // The picture's own watch, above the player: remounts must not hand a
  // dead stage back to silence (see liveness.ts). Waking holds the window.
  const liveness = pictureWatch(
    () => stage,
    (state) => {
      if (state === 'start') {
        if (stalled === 'start') return;
        stalled = 'start';
        rebuild.schedule('start');
        return;
      }
      if (stalled === 'start') stalled = null;
    },
    () => waiting,
  );
  $effect(() => () => liveness.cancel());

  onMount(() => {
    void load();
  });

  async function load() {
    try {
      const page = await api.cameras();
      if (page.needsLogin) {
        onlocked();
        return;
      }
      camera = page.cameras.find((entry) => entry.id === id) ?? null;
      if (!camera) {
        error = 'This camera is not on the account any more.';
        return;
      }
      await start();
      await readPrivacy();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        onlocked();
        return;
      }
      error = err instanceof Error ? err.message : String(err);
    }
  }

  /** Ask the server for this camera's stream of the chosen quality. */
  async function start() {
    const started = await api.watch(id, quality);
    // A freshly minted stream can wake for up to the server's own window;
    // the screen otherwise reads the state once, so it keeps reading
    // while the waking lasts - the player's window must not run against
    // a stream the server is still bringing up.
    waiting = true;
    stream = started.stream;
    attempt += 1;
    void waitOutWaking();
  }

  async function waitOutWaking() {
    while (waiting) {
      await new Promise((resolve) => setTimeout(resolve, 3000));
      if (!waiting) return;
      try {
        const page = await api.cameras();
        const found = page.cameras.find((entry) => entry.id === id) ?? null;
        // The poll is also the screen's only look at the server's own
        // word: a stream that leaves waking into trouble says so here.
        if (found) camera = found;
        waiting = found?.state === 'waking';
      } catch {
        waiting = false;
      }
    }
  }

  /** The person's Try again: a fresh ask, straight away. */
  function retry() {
    rebuild.settled();
    stalled = null;
    waiting = false;
    void start().catch((err) => {
      error = err instanceof Error ? err.message : String(err);
    });
  }

  /** The tab came back and the picture did not follow: a long absence can
   * outlive the stream itself, so ask the server again - the player has
   * already rebuilt itself, and the retry is the person looking at it.
   * A failed re-ask is best-effort and stays off the error banner, the
   * same as the tile's own re-ask: the picture's own stall is what tells
   * the person, and a banner would outlive it. */
  async function recover() {
    try {
      await api.watch(id, quality, true);
    } catch {
      // Deliberately quiet.
    }
  }

  async function setQuality(next: Quality) {
    if (next === quality) return;
    quality = next;
    error = '';
    try {
      await start();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  /** The shadow is the truth about the lens; a failure says so. */
  async function readPrivacy() {
    try {
      privacy = (await api.privacyState(id)).privacy;
    } catch {
      // The control below reports its own failures; a read is best-effort.
    }
  }

  async function togglePrivacy() {
    error = '';
    move = '';
    const want = !(privacy ?? false);
    try {
      const result = await api.privacy(id, want);
      privacy = result.privacy;
      if (!result.ok) {
        error = result.reason ?? 'the camera did not change';
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  /** A drag emits steps faster than the camera answers. One move is in
   * flight at a time, the windows that arrive meanwhile ACCUMULATE (a
   * sustained drag must not lose its middle), and each write stays within
   * the server's per-request cap. */
  const MAX_STEP = 60;
  /** A backlog beyond a few writes is a camera nobody can catch up with. */
  const MAX_QUEUED = 180;

  let moving = false;
  let queued = { dx: 0, dy: 0 };

  function step(dx: number, dy: number) {
    queued = {
      dx: bound(queued.dx + dx, MAX_QUEUED),
      dy: bound(queued.dy + dy, MAX_QUEUED),
    };
    void drain();
  }

  function bound(units: number, limit: number): number {
    return Math.sign(units) * Math.min(Math.abs(units), limit);
  }

  async function drain() {
    if (moving) return;
    moving = true;
    try {
      while (queued.dx !== 0 || queued.dy !== 0) {
        const dx = bound(queued.dx, MAX_STEP);
        const dy = bound(queued.dy, MAX_STEP);
        queued = { dx: queued.dx - dx, dy: queued.dy - dy };
        const result = await api.ptz(id, dx, dy);
        move = result.position ? `at ${result.position.x},${result.position.y}` : '';
        if (!result.ok) {
          // The camera is not following; stop pushing at it.
          move = result.reason ?? 'the move did not land';
          queued = { dx: 0, dy: 0 };
        }
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
      queued = { dx: 0, dy: 0 };
    } finally {
      moving = false;
    }
  }

  /** One frame of what is on screen, as a file. */
  function snapshot() {
    const video = stage?.querySelector('video');
    if (!video) return;
    const canvas = document.createElement('canvas');
    canvas.width = video.videoWidth || 0;
    canvas.height = video.videoHeight || 0;
    if (canvas.width === 0) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
    const link = document.createElement('a');
    const stamp = new Date().toISOString().replace(/[:.]/g, '-');
    link.download = `${camera?.name ?? 'camera'}-${stamp}.png`;
    link.href = canvas.toDataURL('image/png');
    link.click();
  }

  // Audio is negotiated with the stream from the start and starts muted:
  // the player's own control is the one unmute, and nothing is
  // re-negotiated when it is used.
</script>

<main class="screen">
  <header>
    <button class="btn back" onclick={onback}>Back</button>
    <h1>{camera?.name ?? 'Camera'}</h1>
    {#if move}
      <span class="meta">{move}</span>
    {/if}
  </header>

  {#if error}
    <p class="error" role="alert">{error}</p>
  {/if}

  <PtzDrag onstep={step}>
    <div class="screen-stage" bind:this={stage}>
      {#if stream}
        <Player
          {stream}
          {attempt}
          {waiting}
          muted
          onsettled={(state) => {
            waiting = false;
            if (state === 'start' || state === 'frozen') {
              stalled = state;
              rebuild.schedule(state);
              return;
            }
            stalled = null;
            fallingBack = state === 'hls';
            rebuild.settled();
            if (state === 'recovered') void recover();
          }}
        />
      {/if}
      {#if waiting}
        <!-- Waking is said, not just shown: a bare black stage with no
             word is what made a stalled camera read as broken. -->
        <div class="spinner" role="status" aria-label="Starting the stream"></div>
      {/if}
    </div>
  </PtzDrag>

  {#if stalled === 'start'}
    <p class="note">The video did not start. The camera link may be blocked on this network.</p>
    <button class="btn" onclick={retry}>Try again</button>
  {:else if stalled === 'frozen'}
    <p class="note">The picture stopped. The camera link may have dropped.</p>
    <button class="btn" onclick={retry}>Try again</button>
  {:else if fallingBack}
    <!-- The tile already says this; a screen that swapped its picture for
         another and explained nothing read as broken. -->
    <p class="note">WebRTC could not connect here; showing the compatibility stream.</p>
  {:else if camera?.state === 'degraded'}
    <p class="note">{camera.reason ?? 'The stream is having trouble.'}</p>
    <button class="btn" onclick={retry}>Try again</button>
  {/if}

  <div class="controls">
    <button class="btn" onclick={() => setQuality(quality === 'high' ? 'low' : 'high')}>
      {quality === 'high' ? 'High quality' : 'Low quality'}
    </button>
    {#if onrecordings}
      <button class="btn" onclick={onrecordings}>Recordings</button>
    {/if}
    <button class="btn" onclick={togglePrivacy} aria-pressed={privacy === true}>
      {#if privacy === null}
        Privacy
      {:else if privacy}
        Privacy on
      {:else}
        Privacy off
      {/if}
    </button>
    <button class="btn" onclick={snapshot}>Snapshot</button>
  </div>
</main>

<style>
  .screen {
    max-width: 960px;
    margin: 0 auto;
    padding: 20px;
  }

  header {
    display: flex;
    align-items: baseline;
    gap: 12px;
    margin-bottom: 14px;
  }

  h1 {
    font-size: var(--fs-title);
  }

  .meta {
    color: var(--muted);
    font-size: var(--fs-label);
  }

  .screen-stage {
    position: relative;
  }

  .spinner {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    pointer-events: none;
  }

  .spinner::after {
    content: '';
    width: 34px;
    height: 34px;
    border: 3px solid var(--line);
    border-top-color: var(--accent);
    border-radius: 50%;
    animation: spin 0.9s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .spinner::after {
      animation: none;
      border-color: var(--accent);
    }
  }

  .screen-stage :global(.stage) {
    border-radius: var(--radius);
  }

  .controls {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 14px;
  }

  .back {
    font-weight: 600;
  }
</style>
