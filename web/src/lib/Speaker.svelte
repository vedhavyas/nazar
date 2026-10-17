<script lang="ts">
  /** The speaker toggle on a video: muted until the person asks for this
   * camera's sound, and the same shape on every surface. */
  let { on, ontoggle }: { on: boolean; ontoggle: () => void } = $props();
</script>

<button
  class="speaker"
  data-testid="speaker"
  aria-pressed={on}
  aria-label={on ? 'Mute this camera' : 'Listen to this camera'}
  onclick={ontoggle}
>
  <svg viewBox="0 0 24 24" aria-hidden="true">
    <path class="body" d="M4 9v6h4l5 4V5L8 9H4z" />
    {#if on}
      <path class="wave" d="M16.5 8.5a5 5 0 0 1 0 7" />
      <path class="wave" d="M19 6a8.5 8.5 0 0 1 0 12" />
    {:else}
      <path class="wave" d="M16 9l5 6M21 9l-5 6" />
    {/if}
  </svg>
</button>

<style>
  .speaker {
    position: absolute;
    right: 8px;
    bottom: 8px;
    width: 34px;
    height: 34px;
    display: grid;
    place-items: center;
    padding: 0;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: rgba(0, 0, 0, 0.55);
    color: #fff;
    cursor: pointer;
  }

  .speaker:hover {
    background: rgba(0, 0, 0, 0.75);
  }

  .speaker:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  svg {
    width: 20px;
    height: 20px;
  }

  .body {
    fill: currentColor;
  }

  .wave {
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
  }
</style>
