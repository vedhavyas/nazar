<script lang="ts">
  /** A test host with the app's own reactivity: the tile gets its camera
   * from a `$state` here, so a state flip updates the props in place the
   * way the house does - the testing library's rerender swaps the whole
   * props object and cannot show that. */
  import CameraTile from './CameraTile.svelte';

  let { onwatch }: { onwatch: (id: string) => void } = $props();
  let state = $state<'waking' | 'live'>('waking');

  export function settle() {
    state = 'live';
  }
</script>

<CameraTile
  camera={{ id: 'cam-1', name: 'Hall', state, reason: null }}
  stream="cam-1-low"
  {onwatch}
/>
