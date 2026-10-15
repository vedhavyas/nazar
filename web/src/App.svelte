<script lang="ts">
  import { onMount } from 'svelte';

  import { api } from './lib/api';
  import { reachable, watchHealth } from './lib/health';
  import Home from './routes/Home.svelte';
  import Login from './routes/Login.svelte';
  import Unlock from './routes/Unlock.svelte';

  type Screen = 'checking' | 'unlock' | 'login' | 'home';

  let screen = $state<Screen>('checking');

  onMount(() => {
    const stop = watchHealth();
    void boot();
    return stop;
  });

  /** An unlocked, logged-in server answers the camera list straight away. */
  async function boot() {
    try {
      await api.cameras();
      screen = 'home';
    } catch {
      screen = 'unlock';
    }
  }

  async function afterUnlock() {
    try {
      await api.cameras();
      screen = 'home';
    } catch {
      screen = 'login';
    }
  }
</script>

{#if !$reachable}
  <p class="banner" role="status">The server is not answering. Trying again…</p>
{/if}

{#if screen === 'checking'}
  <main class="gate">
    <p class="quiet">Looking for the server…</p>
  </main>
{:else if screen === 'unlock'}
  <Unlock onunlocked={afterUnlock} />
{:else if screen === 'login'}
  <Login onloggedin={() => (screen = 'home')} />
{:else}
  <Home onlocked={() => (screen = 'login')} />
{/if}
