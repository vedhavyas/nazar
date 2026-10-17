<script lang="ts">
  import { onMount } from 'svelte';

  import { ApiError, api, type Camera as CameraInfo, type Quality } from '../lib/api';
  import Player from '../lib/Player.svelte';
  import PtzDrag from '../lib/PtzDrag.svelte';
  import Speaker from '../lib/Speaker.svelte';
  import { audioOn, rememberAudio } from '../lib/audio';
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
  /** Audio starts off: a browser only lets a gesture turn it on, and the
   * choice is remembered per camera. */
  let listening = $state(audioOn(id));
  /** What the last move did, from the shadow. */
  let move = $state('');

  let stage = $state<HTMLDivElement | null>(null);
  let attempt = $state(0);
  /** The server is still waking this stream: the player's start window is
   * held until its own word says otherwise. */
  let waiting = $state(false);

  // A stalled picture heals itself: the screen asks the server again on a
  // bounded backoff, so a dead peer does not park the person on a still.
  const rebuild = rebuildSchedule(() => {
    waiting = false;
    void start().catch(() => {
      // Best-effort, like recover(): the stall itself is what shows.
    });
  });
  $effect(() => () => rebuild.cancel());

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
        waiting = page.cameras.find((entry) => entry.id === id)?.state === 'waking';
      } catch {
        waiting = false;
      }
    }
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

  /** Muting is the element's own video; the speaker is the gesture, and
   * the choice is remembered per camera. */
  function toggleListen() {
    listening = !listening;
    rememberAudio(id, listening);
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

  // Audio is negotiated with the stream from the start and kept muted: a
  // browser only lets a gesture unmute it, and the toggle is that gesture.
  // Nothing is re-negotiated when it flips; the player owns that.
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
          muted={!listening}
          onsettled={(state) => {
            waiting = false;
            if (state === 'start' || state === 'frozen') {
              rebuild.schedule();
              return;
            }
            rebuild.settled();
            if (state === 'recovered') void recover();
          }}
        />
        <Speaker on={listening} ontoggle={toggleListen} />
      {/if}
    </div>
  </PtzDrag>

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
