<script lang="ts">
  import { onMount } from 'svelte';

  import { ApiError, api, inShell } from './lib/api';
  import { reachable, watchHealth } from './lib/health';
  import { createNotifications } from './lib/notifyBridge.svelte';
  import Camera from './routes/Camera.svelte';
  import Home from './routes/Home.svelte';
  import Login from './routes/Login.svelte';
  import Notifications from './routes/Notifications.svelte';
  import Recordings from './routes/Recordings.svelte';
  import Setup from './routes/Setup.svelte';

  type Screen = 'checking' | 'setup' | 'login' | 'home' | 'camera' | 'notifications' | 'recordings';

  let screen = $state<Screen>('checking');
  let openId = $state<string | null>(null);

  // The bell is app-wide: an arrival while any screen is open is told.
  const notes = createNotifications();

  // Looking at the list is seeing: everything is marked as the list shows
  // it.
  $effect(() => {
    if (screen === 'notifications') notes.markSeen();
  });

  onMount(() => {
    const stop = watchHealth();
    const stopNotes = notes.follow();
    void boot();
    void notes.load();
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      document.removeEventListener('visibilitychange', onVisible);
      stop();
      stopNotes();
    };
  });

  /** A login screen left standing by a transient refusal: coming back to
   * the tab re-checks the session, and a clean answer goes home. */
  function onVisible() {
    if (document.hidden || screen !== 'login') return;
    void boot();
  }

  /** A stored session answers the camera list; anything else is login. */
  async function boot() {
    try {
      const page = await api.cameras();
      screen = page.needsLogin ? 'login' : 'home';
    } catch (error) {
      // A shell is not served by the server it calls, so a failure that is
      // not the server's own answer is the address - Setup is where it is
      // fixed. In a browser the server answered the page itself.
      screen = inShell() && !(error instanceof ApiError) ? 'setup' : 'login';
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
{:else if screen === 'setup'}
  <Setup onready={() => void boot()} />
{:else if screen === 'login'}
  <Login onloggedin={() => (screen = 'home')} />
{:else if screen === 'camera' && openId}
  <Camera
    id={openId}
    onback={() => (screen = 'home')}
    onlocked={() => (screen = 'login')}
    onrecordings={() => (screen = 'recordings')}
  />
{:else if screen === 'recordings' && openId}
  <Recordings id={openId} onback={() => (screen = 'camera')} onlocked={() => (screen = 'login')} />
{:else if screen === 'notifications'}
  <Notifications
    entries={notes.entries}
    names={notes.names}
    onback={() => (screen = 'home')}
    onopen={(id) => {
      openId = id;
      screen = 'camera';
    }}
  />
{:else}
  <Home
    unread={notes.unread}
    onnotifications={() => (screen = 'notifications')}
    onopen={(id) => {
      openId = id;
      screen = 'camera';
    }}
    onlocked={() => (screen = 'login')}
  />
{/if}
