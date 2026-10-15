<script lang="ts">
  let { away, onaway }: { away: boolean; onaway: (next: boolean) => void } = $props();
</script>

<label class="setting">
  <input
    type="checkbox"
    checked={away}
    onchange={(event) => onaway(event.currentTarget.checked)}
  />
  <span>Away mode</span>
  <span class="hint">Small tiles show one picture every ten seconds</span>
</label>

<style>
  .setting {
    display: grid;
    grid-template-columns: auto auto;
    align-items: baseline;
    gap: 4px 10px;
    cursor: pointer;
  }

  input {
    width: 16px;
    height: 16px;
    accent-color: var(--accent);
  }

  input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }

  .hint {
    grid-column: 2;
    color: var(--muted);
    font-size: var(--fs-label);
  }
</style>
