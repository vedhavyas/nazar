<script lang="ts">
  import { api, type ProfileChoice } from '../lib/api';

  let { onloggedin }: { onloggedin: () => void } = $props();

  let step = $state<'phone' | 'code' | 'pick'>('phone');
  let phone = $state('');
  let code = $state('');
  let choices = $state<ProfileChoice[]>([]);
  let error = $state('');
  let busy = $state(false);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (busy) return;
    busy = true;
    error = '';
    try {
      if (step === 'phone') {
        await api.requestOtp(phone);
        step = 'code';
      } else if (step === 'code') {
        // A 204 signs one account straight in; several answer the accounts
        // to pick between instead (a number can hold more than one).
        const result = await api.verifyOtp(phone, code);
        if (!result) {
          onloggedin();
        } else if (result.profiles.length) {
          choices = result.profiles;
          step = 'pick';
        } else {
          throw new Error('the server answered no accounts to pick from');
        }
      }
    } catch (err) {
      // The server's own words: a wrong code, a rate limit, whatever it is.
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
    }
  }

  async function pick(uuid: string) {
    if (busy) return;
    busy = true;
    error = '';
    try {
      await api.selectProfile(uuid);
      onloggedin();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
    }
  }

  function changeNumber() {
    step = 'phone';
    code = '';
    choices = [];
    error = '';
  }
</script>

<main class="gate">
  <h1>Sign in</h1>
  <p class="lede">
    {#if step === 'phone'}
      The cameras live behind the family's Qubo account. Nazar will text a code to your phone.
    {:else if step === 'code'}
      Nazar sent a code to {phone}. Type it here.
    {:else}
      This number is on more than one Qubo account. Which one is this?
    {/if}
  </p>

  <form onsubmit={submit}>
    {#if step === 'phone'}
      <label for="phone">Phone number</label>
      <input id="phone" class="mono" type="tel" inputmode="tel" bind:value={phone} />
    {:else if step === 'code'}
      <label for="code">Code</label>
      <input id="code" class="mono" type="text" inputmode="numeric" bind:value={code} />
    {:else}
      <ul class="accounts">
        {#each choices as choice (choice.uuid)}
          <li>
            <button
              class="btn account"
              type="button"
              disabled={busy}
              onclick={() => pick(choice.uuid)}
            >
              <span class="who">{choice.name ?? choice.email ?? choice.uuid}</span>
              {#if choice.name && choice.email}
                <span class="mail">{choice.email}</span>
              {/if}
            </button>
          </li>
        {/each}
      </ul>
    {/if}

    <p class="actions">
      {#if step !== 'pick'}
        <button class="btn go" type="submit" disabled={busy}>
          {step === 'phone' ? 'Send code' : 'Sign in'}
        </button>
      {/if}
      {#if step !== 'phone'}
        <button class="btn" type="button" disabled={busy} onclick={changeNumber}>
          Change number
        </button>
      {/if}
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

  .accounts {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .account {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
    width: 100%;
    text-align: left;
  }

  .mail {
    color: var(--muted);
    font-size: var(--fs-label);
  }
</style>
