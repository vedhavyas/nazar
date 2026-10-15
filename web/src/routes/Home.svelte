<script lang="ts">
  import { onMount } from 'svelte';

  import { ApiError, api, type Camera } from '../lib/api';
  import CameraTile from '../lib/CameraTile.svelte';

  let { onlocked }: { onlocked: () => void } = $props();

  let cameras = $state<Camera[]>([]);
  let error = $state('');
  let loaded = $state(false);

  async function refresh() {
    try {
      cameras = await api.cameras();
      error = '';
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

  async function watch(id: string) {
    error = '';
    try {
      await api.watch(id);
      // Straight to waking so the tap has an answer before the next poll.
      cameras = cameras.map((camera) =>
        camera.id === id ? { ...camera, state: 'waking', reason: null } : camera,
      );
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
    await refresh();
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
    void refresh();
  });
</script>

<main class="home">
  <header>
    <h1>nazar</h1>
  </header>

  {#if error}
    <p class="error" role="alert">{error}</p>
  {/if}

  <div class="tiles">
    {#each cameras as camera (camera.id)}
      <CameraTile {camera} onwatch={watch} />
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
    max-width: 720px;
    margin: 0 auto;
    padding: 28px 20px 48px;
  }

  header {
    margin-bottom: 20px;
  }

  h1 {
    font-size: var(--fs-title);
  }

  .tiles {
    display: grid;
    gap: 16px;
  }
</style>
