<script lang="ts">
  import type { Camera } from './api';

  /** The vendored go2rtc player element (registered in index.html). */
  interface VideoRtcElement extends HTMLElement {
    src: string;
    mode: string;
    media: string;
  }

  let { camera, onwatch }: { camera: Camera; onwatch: (id: string) => void } = $props();

  let host = $state<HTMLDivElement | null>(null);
  let attempt = $state(0);
  let stalled = $state(false);

  const STATE_WORDS: Record<Camera['state'], string> = {
    live: 'Live',
    waking: 'Starting',
    offline: 'Offline',
    degraded: 'Trouble',
  };

  $effect(() => {
    const stage = host;
    if (!stage || camera.state !== 'live' || stalled) return;
    void attempt; // retrying rebuilds the player

    const player = document.createElement('video-rtc') as VideoRtcElement;
    player.className = 'player';
    player.mode = 'webrtc';
    player.media = 'video';
    player.src = `/go2rtc/api/ws?src=${encodeURIComponent(camera.id)}`;
    stage.appendChild(player);

    // WebRTC can fail quietly (a blocked ICE path leaves a black frame);
    // no frame inside the window is the signal the person needs.
    const timer = setTimeout(() => {
      stalled = true;
    }, 12000);
    const video = player.querySelector('video');
    const onplaying = () => clearTimeout(timer);
    video?.addEventListener('playing', onplaying);

    return () => {
      clearTimeout(timer);
      video?.removeEventListener('playing', onplaying);
      player.remove();
    };
  });

  function retry() {
    stalled = false;
    attempt += 1;
    onwatch(camera.id);
  }
</script>

<article class="tile">
  <header>
    <h2>{camera.name}</h2>
    <span class="state" data-state={camera.state}>{STATE_WORDS[camera.state]}</span>
  </header>

  {#if camera.state === 'live'}
    <div class="stage" bind:this={host}></div>
    {#if stalled}
      <p class="note">The video did not start. The camera link may be blocked on this network.</p>
      <button class="btn" onclick={retry}>Try again</button>
    {/if}
  {:else if camera.state === 'degraded'}
    <p class="note">{camera.reason ?? 'The stream is having trouble.'}</p>
    <button class="btn" onclick={() => onwatch(camera.id)}>Try again</button>
  {:else if camera.state === 'waking'}
    <p class="note">Starting the stream…</p>
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

  .stage {
    aspect-ratio: 16 / 9;
    border-radius: 8px;
    overflow: hidden;
    background: #000;
  }

  .stage :global(video-rtc),
  .stage :global(video-rtc video) {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .note {
    margin: 0 0 12px;
    color: var(--muted);
  }
</style>
