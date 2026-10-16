<script lang="ts">
  let { unread, onopen }: { unread: number; onopen: () => void } = $props();
</script>

<button
  class="bell"
  onclick={onopen}
  aria-label={unread > 0 ? `Notifications, ${unread} unread` : 'Notifications'}
>
  <svg viewBox="0 0 24 24" width="21" height="21" aria-hidden="true">
    <path
      d="M12 3a5 5 0 0 0-5 5v3.5L5.5 15h13L17 11.5V8a5 5 0 0 0-5-5Zm-2 14a2 2 0 0 0 4 0"
      fill="none"
      stroke="currentColor"
      stroke-width="1.6"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  </svg>
  {#if unread > 0}
    <span class="count" data-testid="unread">{unread > 99 ? '99+' : unread}</span>
  {/if}
</button>

<style>
  .bell {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 38px;
    height: 38px;
    padding: 0;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
    color: var(--muted);
    cursor: pointer;
  }

  .bell:hover {
    color: var(--text);
  }

  .count {
    position: absolute;
    top: -7px;
    right: -7px;
    min-width: 18px;
    padding: 1px 5px;
    border-radius: 9px;
    background: var(--accent);
    color: var(--bg);
    font-size: 11px;
    font-weight: 620;
    line-height: 1.45;
  }
</style>
