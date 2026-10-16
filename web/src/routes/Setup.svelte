<script lang="ts">
  import { onMount } from 'svelte';

  import { DEFAULT_SERVER, healthAt, serverBase, setServerBase } from '../lib/api';

  let { onready }: { onready: () => void } = $props();

  let address = $state(serverBase() || DEFAULT_SERVER);
  let error = $state('');
  let busy = $state(false);

  onMount(() => {
    // The current address is tried on the way in: the shell opens on the
    // reason the server is missing, never a blank window.
    void probe(address);
  });

  /** Check `candidate`; a server that answers hands the app back, one that
   * does not is shown in the check's own words. */
  async function probe(candidate: string, save = false) {
    if (busy) return;
    busy = true;
    error = '';
    try {
      await healthAt(candidate);
      if (save) setServerBase(candidate);
      onready();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
    }
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    void probe(address, true);
  }
</script>

<main class="gate">
  <h1>Find the server</h1>
  <p class="lede">
    This app talks to the house's own server, over the mesh or the local network. Type where it
    answers; the default is the mesh address.
  </p>

  <form onsubmit={submit}>
    <label for="server">Server address</label>
    <input id="server" class="mono" type="text" inputmode="url" bind:value={address} />

    <p class="actions">
      <button class="btn go" type="submit" disabled={busy}>Connect</button>
    </p>
    {#if error}
      <p class="error" role="alert">{error}</p>
    {/if}
  </form>
</main>

<style>
  .actions {
    display: flex;
    gap: 10px;
    margin: 20px 0 0;
  }
</style>
