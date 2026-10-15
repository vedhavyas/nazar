<script lang="ts">
  /** Drag to pan. A step goes out once per window at most: a finger emits
   * tens of moves a second and the camera does not want them all. */
  let { onstep }: { onstep: (dx: number, dy: number) => void } = $props();

  /** The camera's axes run opposite the screen's (spike, hardware-proven):
   * a leftward drag is a positive x step, an upward drag a positive y. */
  const UNITS_PER_PX = 0.4;
  /** The biggest step one write may carry, in camera units. */
  const MAX_STEP = 60;
  /** Four writes a second, however fast the finger moves. */
  const WRITE_EVERY_MS = 250;
  /** Drags shorter than this are a tap, not a pan. */
  const DEADZONE_PX = 12;
  /** One arrow key press moves the camera as far as a 100px drag. */
  const KEY_PX = 100;

  let dragging = $state(false);
  let hint = $state<string | null>(null);
  let movedX = 0;
  let movedY = 0;
  let lastX = 0;
  let lastY = 0;
  let timer: ReturnType<typeof setInterval> | null = null;

  /** One write, if the window's motion is worth one. */
  function step() {
    const dx = units(-movedX);
    const dy = units(-movedY);
    movedX = 0;
    movedY = 0;
    if (dx === 0 && dy === 0) return;
    onstep(dx, dy);
  }

  function units(px: number): number {
    if (Math.abs(px) < DEADZONE_PX) return 0;
    const rounded = Math.round(px * UNITS_PER_PX);
    return Math.sign(rounded) * Math.min(Math.abs(rounded), MAX_STEP);
  }

  function onpointerdown(event: PointerEvent) {
    dragging = true;
    movedX = 0;
    movedY = 0;
    lastX = event.clientX;
    lastY = event.clientY;
    hint = null;
    (event.currentTarget as HTMLElement).setPointerCapture?.(event.pointerId);
    timer = setInterval(step, WRITE_EVERY_MS);
  }

  function onpointermove(event: PointerEvent) {
    if (!dragging) return;
    movedX += event.clientX - lastX;
    movedY += event.clientY - lastY;
    lastX = event.clientX;
    lastY = event.clientY;
    hint = direction(movedX, movedY);
  }

  function onpointerup() {
    dragging = false;
    if (timer) clearInterval(timer);
    timer = null;
    // The last window's motion landed; the release itself sends nothing.
    movedX = 0;
    movedY = 0;
    hint = null;
  }

  function onkeydown(event: KeyboardEvent) {
    const keys: Record<string, [number, number]> = {
      ArrowLeft: [-1, 0],
      ArrowRight: [1, 0],
      ArrowUp: [0, -1],
      ArrowDown: [0, 1],
    };
    const press = keys[event.key];
    if (!press) return;
    event.preventDefault();
    movedX = press[0] * KEY_PX;
    movedY = press[1] * KEY_PX;
    step();
    hint = null;
  }

  function direction(x: number, y: number): string {
    const parts: string[] = [];
    if (Math.abs(x) >= DEADZONE_PX) parts.push(x < 0 ? 'left' : 'right');
    if (Math.abs(y) >= DEADZONE_PX) parts.push(y < 0 ? 'up' : 'down');
    return parts.join(' ') || 'hold';
  }
</script>

<div
  class="pad"
  class:dragging
  data-testid="ptz-surface"
  tabindex="0"
  aria-label="Drag the picture, or use the arrow keys, to pan and tilt"
  onpointerdown={onpointerdown}
  onpointermove={onpointermove}
  onpointerup={onpointerup}
  onpointercancel={onpointerup}
  onkeydown={onkeydown}
>
  <slot />
  {#if hint}
    <span class="arrow" data-testid="ptz-arrow">{hint}</span>
  {/if}
</div>

<style>
  .pad {
    position: relative;
    touch-action: none;
    cursor: grab;
  }

  .pad.dragging {
    cursor: grabbing;
  }

  .pad:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .arrow {
    position: absolute;
    inset: auto 0 10px 0;
    margin: 0 auto;
    width: fit-content;
    padding: 4px 12px;
    border-radius: 999px;
    background: rgb(0 0 0 / 0.6);
    color: #fff;
    font-size: var(--fs-label);
    pointer-events: none;
  }
</style>
