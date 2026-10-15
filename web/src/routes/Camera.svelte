<script lang="ts">
  import { onMount } from 'svelte';

  import { ApiError, api, type Camera as CameraInfo, type Quality } from '../lib/api';
  import Player from '../lib/Player.svelte';
  import PtzDrag from '../lib/PtzDrag.svelte';

  let { id, onback, onlocked }: { id: string; onback: () => void; onlocked: () => void } =
    $props();

  let camera = $state<CameraInfo | null>(null);
  let quality = $state<Quality>('high');
  let stream = $state<string | null>(null);
  let error = $state('');
  /** The lens state the camera reports, never the ack. `null` is unknown. */
  let privacy = $state<boolean | null>(null);
  /** Audio starts off: a browser only lets a gesture turn it on. */
  let listening = $state(false);
  /** What the last move did, from the shadow. */
  let move = $state('');

  let stage = $state<HTMLDivElement | null>(null);
  let attempt = $state(0);

  const audioKey = () => `nazar.audio.${id}`;

  onMount(() => {
    try {
      listening = localStorage.getItem(audioKey()) === '1';
    } catch {
      // No storage, no remembered setting.
    }
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
    stream = started.stream;
    attempt += 1;
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

  /** A drag emits steps faster than the camera answers; one move is in
   * flight at a time, and only the latest waiting step survives. */
  let moving = false;
  let waiting: { dx: number; dy: number } | null = null;

  async function step(dx: number, dy: number) {
    if (moving) {
      waiting = { dx, dy };
      return;
    }
    moving = true;
    try {
      const result = await api.ptz(id, dx, dy);
      move = result.position ? `at ${result.position.x},${result.position.y}` : '';
      if (!result.ok) {
        move = result.reason ?? 'the move did not land';
      }
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      moving = false;
    }
    const next = waiting;
    waiting = null;
    if (next) {
      void step(next.dx, next.dy);
    }
  }

  /** Muting is the element's own video; the toggle is the gesture. */
  function toggleListen() {
    listening = !listening;
    try {
      localStorage.setItem(audioKey(), listening ? '1' : '0');
    } catch {
      // Not worth an error: the toggle still works for this visit.
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
        <Player {stream} {attempt} muted={!listening} />
      {/if}
    </div>
  </PtzDrag>

  <div class="controls">
    <button class="btn" onclick={() => setQuality(quality === 'high' ? 'low' : 'high')}>
      {quality === 'high' ? 'High quality' : 'Low quality'}
    </button>
    <button class="btn" onclick={togglePrivacy} aria-pressed={privacy === true}>
      {#if privacy === null}
        Privacy
      {:else if privacy}
        Privacy on
      {:else}
        Privacy off
      {/if}
    </button>
    <button class="btn" onclick={toggleListen} aria-pressed={listening}>
      {listening ? 'Listening' : 'Listen'}
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
