<script module lang="ts">
  /** The day's span, in milliseconds: what a scrubber covers. */
  export const DAY_MS = 86_400_000;

  /** Where a scrubber position (0..1) lands on the day's clock. */
  export function timestampAt(dayStart: number, fraction: number): number {
    return dayStart + Math.round(fraction * DAY_MS);
  }

  /** The instant a `DD-MM-YYYY` day begins, in IST - the household's own
   * day, matching the calendar the server asks with `Asia/Kolkata`. */
  export function dayStartMs(day: string): number {
    const [d, m, y] = day.split('-').map(Number);
    return Date.UTC(y, m - 1, d) - (5 * 60 + 30) * 60_000;
  }

  /** A wall-clock hour and minute in IST, whatever the browser's own zone
   * is: the day picker's days are IST days, so the times must be too. */
  export function clockOf(at: number): string {
    const inDay = (((at + (5 * 60 + 30) * 60_000) % DAY_MS) + DAY_MS) % DAY_MS;
    const h = Math.floor(inDay / 3_600_000);
    const m = Math.round(((inDay % 3_600_000) / 60_000) % 60);
    return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}`;
  }
</script>

<script lang="ts">
  import Player from './Player.svelte';

  let {
    stream,
    dayStart,
    at,
    onseek,
    onexit,
  }: {
    stream: string;
    /** The streaming day's first instant, in epoch milliseconds. */
    dayStart: number;
    /** Where the playback currently sits, in epoch milliseconds. */
    at: number;
    /** A new instant to mint: a scrub, a retry, or a recovery. */
    onseek: (timestamp: number) => void;
    /** The player left: the session is the screen's to release. */
    onexit: () => void;
  } = $props();

  let stalled = $state<null | 'start' | 'frozen'>(null);
  let attempt = $state(0);
  /** Where the clock stood when the picture gave out, in seconds. */
  let playhead = $state<number | null>(null);

  /** The scrubber's thumb, from the current instant. */
  const fraction = $derived(Math.min(1, Math.max(0, (at - dayStart) / DAY_MS)));

  /** Recover by asking the server for the instant the person is actually
   * at: the session underneath may have died while nobody consumed it, and
   * a re-ask belongs at the playhead, not at the clip's beginning. */
  function retry() {
    stalled = null;
    attempt += 1;
    onseek(at + Math.round((playhead ?? 0) * 1000));
    playhead = null;
  }
</script>

<div class="clip">
  <Player
    {stream}
    {attempt}
    muted
    onsettled={(state, clock) => {
      if (clock !== undefined) playhead = clock;
      if (state === 'hls' || state === 'recovered') {
        stalled = null;
        if (state === 'recovered') retry();
        return;
      }
      stalled = state;
    }}
  />
  {#if stalled === 'start'}
    <p class="note">The recording did not start. The playback session may have ended.</p>
    <button class="btn" onclick={retry}>Try again</button>
  {:else if stalled === 'frozen'}
    <p class="note">The picture stopped. The playback session may have ended.</p>
    <button class="btn" onclick={retry}>Try again</button>
  {/if}
  <input
    class="scrub"
    type="range"
    min="0"
    max="1000"
    value={Math.round(fraction * 1000)}
    data-testid="scrub"
    aria-label="Seek within the day"
    onchange={(event) => {
      stalled = null;
      playhead = null;
      onseek(timestampAt(dayStart, Number(event.currentTarget.value) / 1000));
    }}
  />
  <div class="foot">
    <time>{clockOf(at)}</time>
    <button class="btn" onclick={onexit}>Close</button>
  </div>
</div>

<style>
  .clip {
    display: grid;
    gap: 10px;
    margin-top: 16px;
  }

  .scrub {
    width: 100%;
    accent-color: var(--accent);
  }

  .foot {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }

  time {
    color: var(--muted);
    font-family: var(--mono);
  }

  .note {
    margin: 0;
    color: var(--muted);
  }
</style>
