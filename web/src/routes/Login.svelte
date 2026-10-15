<script lang="ts">
  import { api } from '../lib/api';

  let { onloggedin }: { onloggedin: () => void } = $props();

  let step = $state<'phone' | 'code'>('phone');
  let phone = $state('');
  let code = $state('');
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
      } else {
        await api.verifyOtp(phone, code);
        onloggedin();
      }
    } catch (err) {
      // The server's own words: a wrong code, a rate limit, whatever it is.
      error = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
    }
  }

  function changeNumber() {
    step = 'phone';
    code = '';
    error = '';
  }
</script>

<main class="gate">
  <h1>Sign in</h1>
  <p class="lede">
    {#if step === 'phone'}
      The cameras live behind the family's Qubo account. We will text a code to your phone.
    {:else}
      Enter the code we sent to {phone}.
    {/if}
  </p>

  <form onsubmit={submit}>
    {#if step === 'phone'}
      <label for="phone">Phone number</label>
      <input id="phone" class="mono" type="tel" inputmode="tel" bind:value={phone} />
    {:else}
      <label for="code">Code</label>
      <input id="code" class="mono" type="text" inputmode="numeric" bind:value={code} />
    {/if}

    <p class="actions">
      <button class="btn go" type="submit" disabled={busy}>
        {step === 'phone' ? 'Send code' : 'Sign in'}
      </button>
      {#if step === 'code'}
        <button class="btn" type="button" onclick={changeNumber}>Change number</button>
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
</style>
