<script lang="ts">
  import type { Camera } from './api';
  import Player from './Player.svelte';

  let {
    camera,
    stream,
    onwatch,
    onopen,
    still = false,
  }: {
    camera: Camera;
    /** The go2rtc stream name the server minted for this tile. */
    stream?: string;
    onwatch: (id: string) => void;
    onopen?: (id: string) => void;
    still?: boolean;
  } = $props();

  let picture = $state<HTMLDivElement | null>(null);
  let snapshot = $state<HTMLCanvasElement | null>(null);
  let attempt = $state(0);
  /** Why the picture is not moving, when it is not. */
  let stalled = $state<null | 'start' | 'frozen'>(null);
  /** Whether the fallback stream is carrying the picture. */
  let fallingBack = $state(false);

  const STATE_WORDS: Record<Camera['state'], string> = {
    live: 'Live',
    waking: 'Starting',
    offline: 'Offline',
    degraded: 'Trouble',
  };

  /** How often away mode refreshes a tile's still. */
  const STILL_EVERY_MS = 10_000;

  // Away mode: the tile keeps its own stream, and shows one frame of it
  // every ten seconds rather than a live picture - no extra server work.
  $effect(() => {
    if (!still || camera.state !== 'live' || stalled) return;
    const canvas = snapshot;
    if (!canvas) return;
    const draw = () => {
      const video = picture?.querySelector('video');
      const ctx = canvas.getContext('2d');
      if (!video || !ctx) return;
      canvas.width = video.videoWidth || canvas.width || 640;
      canvas.height = video.videoHeight || canvas.height || 360;
      ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
    };
    draw();
    const timer = setInterval(draw, STILL_EVERY_MS);
    return () => clearInterval(timer);
  });

  function retry() {
    stalled = null;
    attempt += 1;
    onwatch(camera.id);
  }
</script>

<article class="tile">
  <header>
    {#if onopen}
      <button class="name" onclick={() => onopen(camera.id)}>{camera.name}</button>
    {:else}
      <h2>{camera.name}</h2>
    {/if}
    <span class="state" data-state={camera.state}>{STATE_WORDS[camera.state]}</span>
  </header>

  {#if camera.state === 'live' && stream}
    <div class="picture" class:hidden={still && !stalled} bind:this={picture}>
      <Player
        {stream}
        {attempt}
        muted
        onsettled={(state) => {
          if (state === 'hls' || state === 'recovered') {
            fallingBack = state === 'hls';
            stalled = null;
            return;
          }
          fallingBack = false;
          stalled = state;
        }}
      />
    </div>
    {#if still && !stalled}
      <canvas class="still" bind:this={snapshot}></canvas>
    {/if}
    {#if stalled === 'start'}
      <p class="note">The video did not start. The camera link may be blocked on this network.</p>
      <button class="btn" onclick={retry}>Try again</button>
    {:else if stalled === 'frozen'}
      <p class="note">The picture stopped. The camera link may have dropped.</p>
      <button class="btn" onclick={retry}>Try again</button>
    {:else if fallingBack && !still}
      <p class="note">WebRTC could not connect here; showing the compatibility stream.</p>
    {/if}
  {:else if camera.state === 'degraded'}
    <p class="note">{camera.reason ?? 'The stream is having trouble.'}</p>
    <button class="btn" onclick={() => onwatch(camera.id)}>Try again</button>
  {:else if camera.state === 'waking' || camera.state === 'live'}
    <!-- Live but with no stream name yet: the mint is still landing. -->
    <p class="note">Starting the stream...</p>
  {:else}
    <p class="note">Not streaming.</p>
    <button class="btn go" onclick={() => onwatch(camera.id)}>Watch</button>
  {/if}
</article>

<style>
  .tile {
    padding: 14px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
  }

  header {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 12px;
  }

  h2 {
    font-size: 17px;
  }

  /* The name is the door to the camera's own screen. */
  .name {
    padding: 0;
    border: 0;
    background: none;
    color: inherit;
    font: inherit;
    font-size: 17px;
    font-weight: 600;
    cursor: pointer;
    text-decoration: underline;
    text-decoration-color: var(--line);
    text-underline-offset: 4px;
  }

  .name:hover {
    text-decoration-color: var(--accent);
  }

  .name:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .state {
    color: var(--muted);
    font-size: var(--fs-label);
  }

  .state[data-state='live'] {
    color: var(--accent);
  }

  .state[data-state='degraded'] {
    color: var(--bad);
  }

  /* Away mode hides the live picture behind the still it feeds. */
  .picture.hidden {
    display: none;
  }

  .still {
    display: block;
    width: 100%;
    aspect-ratio: 16 / 9;
    border-radius: 8px;
    background: #000;
  }

  .note {
    margin: 0 0 12px;
    color: var(--muted);
  }
</style>
