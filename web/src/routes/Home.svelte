<script lang="ts">
  import { onMount } from 'svelte';

  import { ApiError, api, type Camera, type Quality } from '../lib/api';
  import CameraTile from '../lib/CameraTile.svelte';
  import Settings from '../lib/Settings.svelte';
  import { createHero } from '../lib/useHero.svelte';

  let { onlocked, onopen }: { onlocked: () => void; onopen: (id: string) => void } = $props();

  let cameras = $state<Camera[]>([]);
  let error = $state('');
  let loaded = $state(false);

  // The hero is the server's decision; this side only follows it.
  const house = createHero(() => cameras);
  const AWAY_KEY = 'nazar.away';

  async function refresh() {
    try {
      const page = await api.cameras();
      cameras = page.cameras;
      error = '';
      // Qubo refused the session: only a fresh login revives the streams.
      if (page.needsLogin) {
        onlocked();
        return;
      }
      await house.refresh();
      void startStreams();
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        onlocked();
        return;
      }
      error = err instanceof Error ? err.message : String(err);
    } finally {
      loaded = true;
    }
  }

  /** The hero streams high, the tiles low; anything already live is left. */
  async function startStreams() {
    for (const tile of house.tiles) {
      if (tile.camera.state === 'live' || tile.camera.state === 'waking') continue;
      await watch(tile.camera.id, tile.hero ? 'high' : 'low');
    }
  }

  async function watch(id: string, quality: Quality) {
    try {
      await api.watch(id, quality);
      cameras = cameras.map((camera) =>
        camera.id === id ? { ...camera, state: 'waking', reason: null } : camera,
      );
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  }

  /** A tile asks for the stream its role and its own state imply. */
  function retile(id: string) {
    const tile = house.tiles.find((entry) => entry.camera.id === id);
    void watch(id, tile?.hero === true ? 'high' : 'low').then(refresh);
  }

  function setAway(next: boolean) {
    house.setAway(next);
    try {
      localStorage.setItem(AWAY_KEY, next ? '1' : '0');
    } catch {
      // A browser that refuses storage just forgets the preference.
    }
  }

  // Poll only while something is in motion; a settled house needs nothing.
  $effect(() => {
    const moving = cameras.some(
      (camera) => camera.state === 'waking' || camera.state === 'degraded',
    );
    if (!moving) return;
    const timer = setInterval(() => void refresh(), 3000);
    return () => clearInterval(timer);
  });

  onMount(() => {
    try {
      house.setAway(localStorage.getItem(AWAY_KEY) === '1');
    } catch {
      // No storage, no remembered preference.
    }
    const stop = house.follow();
    void refresh();
    return stop;
  });
</script>

<main class="home">
  <header>
    <h1>nazar</h1>
    <Settings away={house.away} onaway={setAway} />
  </header>

  {#if error}
    <p class="error" role="alert">{error}</p>
  {/if}

  <div class="house">
    {#each house.tiles as tile (tile.camera.id)}
      <div class:hero={tile.hero}>
        <CameraTile camera={tile.camera} onwatch={retile} onopen={onopen} still={tile.still} />
      </div>
    {/each}
  </div>

  {#if loaded && cameras.length === 0}
    <p class="quiet">
      No cameras on this account yet. Once a camera is paired with the Qubo app it shows up here.
    </p>
  {/if}
</main>

<style>
  .home {
    max-width: 960px;
    margin: 0 auto;
    padding: 28px 20px 48px;
  }

  header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 16px;
    flex-wrap: wrap;
    margin-bottom: 20px;
  }

  h1 {
    font-size: var(--fs-title);
  }

  .house {
    display: grid;
    gap: 16px;
    grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
  }

  /* The hero takes the full row above the tiles. */
  .hero {
    grid-column: 1 / -1;
  }
</style>
