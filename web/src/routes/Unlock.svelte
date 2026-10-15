<script lang="ts">
  import { ApiError, api } from '../lib/api';

  let { onunlocked }: { onunlocked: () => void } = $props();

  let pin = $state('');
  let error = $state('');
  let busy = $state(false);
  let setting = $state(false);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    error = '';
    try {
      if (setting) {
        await api.setPin(pin);
        setting = false;
      }
      await api.unlock(pin);
      onunlocked();
    } catch (err) {
      // 409 from unlock means no PIN exists yet: first run, so set one.
      if (!setting && err instanceof ApiError && err.status === 409) {
        setting = true;
        error = '';
      } else {
        error = err instanceof Error ? err.message : String(err);
      }
    } finally {
      busy = false;
    }
  }
</script>

<main class="gate">
  <h1>nazar</h1>
  <p class="lede">
    {#if setting}
      First time on this server - choose the house PIN.
    {:else}
      Enter the house PIN to see the cameras.
    {/if}
  </p>

  <form onsubmit={submit}>
    <label for="pin">{setting ? 'New PIN' : 'PIN'}</label>
    <input
      id="pin"
      class="mono"
      type="password"
      inputmode="numeric"
      autocomplete={setting ? 'new-password' : 'current-password'}
      bind:value={pin}
    />
    <p class="actions">
      <button class="btn go" type="submit" disabled={busy || pin.length === 0}>
        {setting ? 'Set PIN and continue' : 'Unlock'}
      </button>
    </p>
    {#if error}
      <p class="error" role="alert">{error}</p>
    {/if}
  </form>
</main>

<style>
  .actions {
    margin: 20px 0 0;
  }
</style>
