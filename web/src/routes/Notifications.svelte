<script lang="ts">
  import type { Notification } from '../lib/api';
  import { describe, since } from '../lib/notifyBridge.svelte';

  let {
    entries,
    names,
    onback,
    onopen,
  }: {
    entries: Notification[];
    names: Record<string, string>;
    onback: () => void;
    onopen: (id: string) => void;
  } = $props();

  /** The kind's word, for the block an entry without a still shows. */
  const KIND_WORDS: Record<Notification['kind'], string> = {
    motion: 'Motion',
    offline: 'Offline',
    online: 'Online',
    aqi: 'Air',
  };

  const now = Date.now();
</script>

{#snippet told(entry: Notification)}
  {#if entry.snapshot_url}
    <img class="still" src={entry.snapshot_url} alt="" />
  {:else}
    <span class="noshot" aria-hidden="true">{KIND_WORDS[entry.kind]}</span>
  {/if}
  <span class="words">{describe(entry, names)}</span>
  <time class="when">{since(entry.at, now)}</time>
{/snippet}

<main class="told">
  <header>
    <button class="back" onclick={onback}>Back</button>
    <h1>Notifications</h1>
  </header>

  {#if entries.length === 0}
    <p class="quiet">
      Nothing yet. Motion, cameras coming and going, and air quality land here.
    </p>
  {:else}
    <ul>
      {#each entries as entry (entry.id)}
        {@const camera = names[entry.device_id]}
        <li>
          {#if camera}
            <button class="row" onclick={() => onopen(entry.device_id)}>
              {@render told(entry)}
            </button>
          {:else}
            <div class="row">{@render told(entry)}</div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</main>

<style>
  .told {
    max-width: 640px;
    margin: 0 auto;
    padding: 28px 20px 48px;
  }

  header {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-bottom: 20px;
  }

  h1 {
    font-size: var(--fs-title);
  }

  .back {
    padding: 8px 14px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
    color: var(--text);
    font: inherit;
    cursor: pointer;
  }

  ul {
    display: grid;
    gap: 10px;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .row {
    display: grid;
    grid-template-columns: 56px 1fr auto;
    align-items: center;
    gap: 14px;
    width: 100%;
    padding: 10px 14px 10px 10px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
    color: inherit;
    font: inherit;
    text-align: left;
  }

  button.row {
    cursor: pointer;
  }

  button.row:hover {
    border-color: var(--accent);
  }

  .still {
    display: block;
    width: 56px;
    height: 40px;
    border-radius: 6px;
    object-fit: cover;
    background: #000;
  }

  /* No still is a deliberate block, not a broken picture. */
  .noshot {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 56px;
    height: 40px;
    border: 1px solid var(--line);
    border-radius: 6px;
    color: var(--muted);
    font-size: 11px;
  }

  .when {
    color: var(--muted);
    font-size: var(--fs-label);
    white-space: nowrap;
  }
</style>
