<script lang="ts">
  import { onMount } from 'svelte';

  import { api } from './lib/api';
  import { reachable, watchHealth } from './lib/health';
  import Home from './routes/Home.svelte';
  import Login from './routes/Login.svelte';

  type Screen = 'checking' | 'login' | 'home';

  let screen = $state<Screen>('checking');

  onMount(() => {
    const stop = watchHealth();
    void boot();
    return stop;
  });

  /** A stored session answers the camera list; anything else is login. */
  async function boot() {
    try {
      const page = await api.cameras();
      screen = page.needsLogin ? 'login' : 'home';
    } catch {
      screen = 'login';
    }
  }
</script>

{#if !$reachable}
  <p class="banner" role="status">The server is not answering. Trying again...</p>
{/if}

{#if screen === 'checking'}
  <main class="gate">
    <p class="quiet">Looking for the server...</p>
  </main>
{:else if screen === 'login'}
  <Login onloggedin={() => (screen = 'home')} />
{:else}
  <Home onlocked={() => (screen = 'login')} />
{/if}
