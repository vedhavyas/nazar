<script lang="ts">
  /** Press and hold to talk through the camera. Shells only: the web build
   * has no microphone path, so the button renders nothing there. When the
   * fast lane is not armed the button is disabled with the reason - a held
   * button must never silently swallow audio - and a lane that drops
   * mid-hold releases the talk. */
  let {
    armed,
    reason = 'talkback needs the fast lane',
    onstart,
    onstop,
  }: {
    armed: boolean;
    reason?: string;
    onstart: () => void;
    onstop: () => void;
  } = $props();

  /** Tauri shells inject this global; the browser does not. */
  const shell = typeof window !== 'undefined' && '__TAURI__' in window;

  let talking = $state(false);

  function press() {
    if (!armed || talking) return;
    talking = true;
    onstart();
  }

  function release() {
    if (!talking) return;
    talking = false;
    onstop();
  }

  // A lane that drops mid-hold must end the burst, not swallow it.
  $effect(() => {
    if (!armed) release();
  });
</script>

{#if shell}
  <button
    type="button"
    class="talk"
    data-testid="talk-button"
    disabled={!armed}
    title={armed ? 'Hold to talk' : reason}
    onpointerdown={press}
    onpointerup={release}
    onpointerleave={release}
    onpointercancel={release}
  >
    {#if !armed}
      {reason}
    {:else if talking}
      Talking...
    {:else}
      Hold to talk
    {/if}
  </button>
{/if}

<style>
  .talk {
    align-self: center;
    padding: 0.5rem 1.25rem;
    border: 1px solid var(--line, #444);
    border-radius: 999px;
    background: transparent;
    color: inherit;
    font: inherit;
    cursor: pointer;
    user-select: none;
    touch-action: none;
  }

  .talk:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .talk:active:not(:disabled) {
    background: var(--accent, #3d5afe);
    border-color: var(--accent, #3d5afe);
  }
</style>
