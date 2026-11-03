<script lang="ts">
  /* A stand-in for the grid tile: Home tests read the camera state it is
     handed, which the real tile never renders (the detail stays off the
     grid by design). */
  import type { Camera } from './api';

  let {
    camera,
    onwatch,
  }: {
    camera: Camera;
    stream?: string;
    onwatch: (id: string) => void;
    onopen?: (id: string) => void;
    still?: boolean;
  } = $props();
</script>

<button onclick={() => onwatch(camera.id)}>retile</button>
<pre data-testid="camera">{JSON.stringify(camera)}</pre>
