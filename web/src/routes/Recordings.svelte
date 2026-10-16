<script lang="ts">
  import { onDestroy, onMount, untrack } from 'svelte';

  import { ApiError, api, type Camera, type Clip } from '../lib/api';
  import ClipPlayer, { clockOf, dayStartMs } from '../lib/ClipPlayer.svelte';

  let {
    id,
    onback,
    onlocked,
  }: { id: string; onback: () => void; onlocked: () => void } = $props();

  let cameras = $state<Camera[]>([]);
  /** Seeded from the camera that was opened, then the tabs' own; untrack
   * says the seed is deliberate, not a missed dependency. */
  let camera = $state(untrack(() => id));
  let error = $state('');
  let days = $state<string[]>([]);
  let day = $state<string | null>(null);
  let clips = $state<Clip[]>([]);
  let loaded = $state(false);
  /** The running playback: the stream, where it sits, and whose it is -
   * the release must go to the camera the session was minted for, not
   * whichever tab is showing when the player leaves. */
  let playing = $state<{ stream: string; at: number; camera: string } | null>(null);

  /** How far back the day picker looks. */
  const DAYS_WINDOW_MS = 30 * 86_400_000;

  onMount(() => {
    void load();
  });

  onDestroy(() => {
    // The screen is going away with a session open: releasing here makes
    // the exit path work however it was left.
    void stop();
  });

  async function load() {
    try {
      const page = await api.cameras();
      if (page.needsLogin) {
        onlocked();
        return;
      }
      cameras = page.cameras;
      await loadDays();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        onlocked();
        return;
      }
      error = err instanceof Error ? err.message : String(err);
    }
  }

  async function loadDays() {
    error = '';
    try {
      const to = Date.now();
      days = (await api.recordedDays(camera, to - DAYS_WINDOW_MS, to)).days;
      day = days.at(-1) ?? null;
      await loadClips();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      loaded = true;
    }
  }

  async function loadClips() {
    if (!day) {
      clips = [];
      return;
    }
    try {
      clips = (await api.recordings(camera, day)).clips;
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  async function pick(next: string) {
    if (next === camera) return;
    await stop();
    camera = next;
    days = [];
    clips = [];
    loaded = false;
    await loadDays();
  }

  /** Bumped whenever the screen moves on - a camera switch, a departure,
   * a newer seek: a mint that lands after it is nobody's. */
  let generation = 0;

  /** Start (or move) the playback: a mint at the instant asked for. */
  async function seek(timestamp: number) {
    error = '';
    const mine = ++generation;
    const forCamera = camera;
    try {
      const started = await api.playback(forCamera, timestamp);
      if (mine !== generation) {
        // The screen moved on while the mint was in flight: the session is
        // nobody's, and it is released against the camera it was minted
        // for - never the one showing now.
        await release(forCamera);
        return;
      }
      playing = { stream: started.stream, at: timestamp, camera: forCamera };
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  /** Release one camera's session, whatever this screen is showing now. */
  async function release(forCamera: string) {
    try {
      await api.playbackStop(forCamera);
    } catch {
      // The session still idles out on the server; nothing to tell here.
    }
  }

  /** The player left: release the session rather than leave it to idle. */
  async function stop() {
    generation += 1;
    const open = playing;
    playing = null;
    if (open) {
      await release(open.camera);
    }
  }

  function leave() {
    void stop().then(onback);
  }
</script>

<main class="recordings">
  <header>
    <button class="btn" onclick={leave}>Back</button>
    <h1>Recordings</h1>
  </header>

  {#if error}
    <p class="error" role="alert">{error}</p>
  {/if}

  <div class="tabs">
    {#each cameras as entry (entry.id)}
      <button class="tab" class:on={entry.id === camera} onclick={() => pick(entry.id)}>
        {entry.name}
      </button>
    {/each}
  </div>

  {#if days.length > 0}
    <div class="daypick">
      <label for="day">Day</label>
      <select
        id="day"
        value={day}
        onchange={(event) => {
          day = event.currentTarget.value;
          void loadClips();
        }}
      >
        {#each [...days].reverse() as entry (entry)}
          <option value={entry}>{entry}</option>
        {/each}
      </select>
    </div>
  {:else if loaded}
    <p class="quiet">No recordings in the last 30 days.</p>
  {/if}

  {#if clips.length === 0 && day && loaded}
    <p class="quiet">Nothing moved on this day.</p>
  {/if}

  <ul>
    {#each clips as clip (clip.id)}
      <li>
        <button class="entry" onclick={() => seek(clip.start_ms)}>
          {#if clip.snapshot_url}
            <img class="still" src={clip.snapshot_url} alt="" />
          {:else}
            <span class="noshot" aria-hidden="true">Clip</span>
          {/if}
          <span class="words">{clockOf(clip.start_ms)}</span>
        </button>
      </li>
    {/each}
  </ul>

  {#if playing && day}
    <ClipPlayer
      stream={playing.stream}
      dayStart={dayStartMs(day)}
      at={playing.at}
      onseek={(timestamp) => void seek(timestamp)}
      onexit={leave}
    />
  {/if}
</main>

<style>
  .recordings {
    max-width: 720px;
    margin: 0 auto;
    padding: 28px 20px 48px;
  }

  header {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-bottom: 20px;
  }

  h1 {
    font-size: var(--fs-title);
  }

  .tabs {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 14px;
  }

  .tab {
    padding: 8px 14px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
    color: var(--muted);
    font: inherit;
    cursor: pointer;
  }

  .tab.on {
    border-color: var(--accent);
    color: var(--text);
  }

  .daypick {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 14px;
  }

  label {
    margin: 0;
  }

  select {
    padding: 8px 12px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
    color: var(--text);
    font: inherit;
  }

  ul {
    display: grid;
    gap: 10px;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .entry {
    display: grid;
    grid-template-columns: 56px 1fr;
    align-items: center;
    gap: 14px;
    width: 100%;
    padding: 10px 14px 10px 10px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }

  .entry:hover {
    border-color: var(--accent);
  }

  .still {
    display: block;
    width: 56px;
    height: 40px;
    border-radius: 6px;
    object-fit: cover;
    background: #000;
  }

  /* No still is a deliberate block, not a broken picture. */
  .noshot {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 56px;
    height: 40px;
    border: 1px solid var(--line);
    border-radius: 6px;
    color: var(--muted);
    font-size: 11px;
  }

  .words {
    font-family: var(--mono);
  }
</style>
