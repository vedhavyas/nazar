<script lang="ts">
  /** The picture: go2rtc's WebRTC signalling, with HLS as the fallback for
   * a network where the socket cannot negotiate (design §5). */
  import { untrack } from 'svelte';
  import { serverBase } from './api';

  interface VideoRtcElement extends HTMLElement {
    src: string;
    mode: string;
    media: string;
  }

  let {
    stream,
    muted = true,
    attempt = 0,
    waiting = false,
    onsettled,
  }: {
    stream: string;
    muted?: boolean;
    /** Bump this to start the picture over from the WebRTC socket. */
    attempt?: number;
    /** The server is still bringing the stream up (`waking`): the start
     * window must not run yet - there is no picture to negotiate - and it
     * opens the moment the state leaves waking. The server's own window
     * can outlast this player's many times over. */
    waiting?: boolean;
    /** Told when the picture was not there: 'start' for a socket that never
     * negotiated (and no HLS to fall back to), 'frozen' for one that
     * stopped, 'hls' for a fallback that took over, 'recovered' for a
     * picture that was started again on coming back to the tab. The
     * playhead rides along where one exists, in seconds: a re-ask belongs
     * where the person actually was, not where the stream began. */
    onsettled?: (
      state: 'start' | 'frozen' | 'hls' | 'recovered',
      playhead?: number,
    ) => void;
  } = $props();

  /** How long a first frame may take before WebRTC is given up on. */
  const START_WINDOW_MS = 12_000;
  /** How often an advancing clock is looked for once the picture starts. */
  const WATCH_EVERY_MS = 5_000;

  /** hls.js as the vendored file exposes it: `isSupported` is on the
   * constructor, everything else on an instance. */
  interface HlsInstance {
    loadSource: (source: string) => void;
    attachMedia: (video: HTMLVideoElement) => void;
    destroy: () => void;
  }

  interface HlsConstructor {
    new (): HlsInstance;
    isSupported?: () => boolean;
  }

  let host = $state<HTMLDivElement | null>(null);
  /** Arms the start window once the server stops waking; installed by the
   * setup effect so the waiting flip never rebuilds the player. */
  let armStartFor = $state<(() => void) | null>(null);
  let fallback = $state(false);
  /** Bumped when the picture is started again after the tab came back. */
  let rebuilt = $state(0);
  /** The browser plays HLS itself; nothing needs loading. */
  let native = $state(true);
  let picture = $state<HTMLVideoElement | null>(null);

  let loading: Promise<void> | null = null;

  /** Fetch the vendored player only when it is needed: a browser that
   * plays HLS itself, or a WebRTC path that works, never loads it. */
  function loadHlsJs(): Promise<void> {
    if (hlsJs()) return Promise.resolve();
    loading ??= new Promise<void>((resolve, reject) => {
      const script = document.createElement('script');
      script.src = '/hls.min.js';
      script.onload = () => resolve();
      script.onerror = () => reject(new Error('the hls player did not load'));
      document.head.appendChild(script);
    }).catch((err: unknown) => {
      // A failed fetch is not remembered: the retry may fetch it again.
      loading = null;
      throw err;
    });
    return loading;
  }

  function hlsJs(): HlsConstructor | undefined {
    return (globalThis as { Hls?: HlsConstructor }).Hls;
  }

  /** Give the picture to the fallback: native HLS where the browser has it,
   * the vendored player where it does not. Returns whether one took it. */
  async function engageFallback(): Promise<boolean> {
    if (playsHls()) {
      native = true;
      fallback = true;
      return true;
    }
    try {
      await loadHlsJs();
    } catch {
      return false;
    }
    if (!hlsJs()?.isSupported?.()) {
      return false;
    }
    native = false;
    fallback = true;
    return true;
  }

  /** Safari, iOS and Chrome-on-Android play HLS themselves; desktop
   * Chromium and Firefox do not, and there the retry is the fallback. */
  function playsHls(): boolean {
    const probe = document.createElement('video');
    return probe.canPlayType('application/vnd.apple.mpegurl') !== '';
  }

  $effect(() => {
    const stage = host;
    const name = stream;
    if (!stage || !name || fallback) return;
    void attempt; // retrying rebuilds the player
    void rebuilt; // so does coming back to a picture that did not

    const player = document.createElement('video-rtc') as VideoRtcElement;
    player.className = 'player';
    player.mode = 'webrtc';
    // Audio is negotiated with the stream and kept muted until a gesture
    // says otherwise; nothing re-negotiates when the mute flips.
    player.media = 'video,audio';
    player.src = `${serverBase()}/go2rtc/api/ws?src=${encodeURIComponent(name)}`;
    stage.appendChild(player);

    // WebRTC fails quietly: a blocked ICE path leaves a black frame, and a
    // dead stream leaves the last one. No first frame inside the window,
    // then no advancing clock, are the two signals a person can act on.
    const stall = (state: 'start' | 'frozen', playhead?: number) => {
      if (state === 'frozen') {
        // A painted picture that stopped holds its last frame: it resumes
        // in place when the server heals, and reaching for the fallback
        // would replace the frame with a second dead player under it.
        frozenHeld = true;
        onsettled?.('frozen', playhead);
        return;
      }
      void engageFallback().then((taken) => {
        onsettled?.(taken ? 'hls' : state, playhead);
      });
    };
    /** A frozen picture was announced; the next playing clears it. */
    let frozenHeld = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let watching: ReturnType<typeof setInterval> | null = null;
    let lastTime = -1;
    let playing = false;
    /** The person paused it; a standing clock is their doing. */
    let paused = false;
    /** Where the clock stood when the tab was hidden, if it had started. */
    let awayAt = -1;

    /** Open the start window. Held back while the server is still waking
     * (there is no picture to negotiate yet, and its window can outlast
     * this one many times over); the waiting effect arms it then. */
    const armStart = () => {
      if (timer || playing) return;
      timer = setTimeout(() => stall('start'), START_WINDOW_MS);
    };
    if (untrack(() => waiting)) {
      armStartFor = armStart;
    } else {
      armStart();
    }

    const tick = () => {
      const video = player.querySelector('video');
      if (!video) return;
      if (paused) {
        // A paused player is not a frozen one: the clock stands still
        // because the person asked it to.
        lastTime = video.currentTime;
        return;
      }
      if (video.currentTime <= lastTime) {
        if (watching) clearInterval(watching);
        watching = null;
        stall('frozen', video.currentTime);
        return;
      }
      lastTime = video.currentTime;
    };
    const watch = () => {
      if (watching) return;
      watching = setInterval(tick, WATCH_EVERY_MS);
    };
    const onplaying = () => {
      playing = true;
      paused = false;
      if (timer) clearTimeout(timer);
      timer = null;
      if (frozenHeld) {
        // The picture came back on its own: the held frame resumes and
        // the quiet note clears, all in place.
        frozenHeld = false;
        onsettled?.('recovered');
      }
      watch();
    };
    const onpause = () => {
      paused = true;
    };

    // A background tab is not a stalled picture: Chromium throttles it and
    // may drop the peer behind its back, so the watch stands down while it
    // cannot see. On return the clock is given a beat before the peer is
    // believed dead: the browser suspends media in hidden tabs routinely,
    // and rebuilding on that alone flashed a reset on every return.
    let grace: ReturnType<typeof setInterval> | null = null;
    const onVisibility = () => {
      const video = player.querySelector('video');
      if (document.hidden) {
        awayAt = playing && video ? video.currentTime : -1;
        if (timer) clearTimeout(timer);
        timer = null;
        if (watching) clearInterval(watching);
        watching = null;
        return;
      }
      const here = video ? video.currentTime : -1;
      if (!playing || (awayAt >= 0 && here <= awayAt)) {
        if (grace) return;
        let tries = 0;
        grace = setInterval(() => {
          tries += 1;
          const now = player.querySelector('video')?.currentTime ?? -1;
          if (now > here) {
            // The tab resumed on its own: same player, same picture.
            if (grace) clearInterval(grace);
            grace = null;
            awayAt = -1;
            lastTime = -1;
            watch();
            return;
          }
          if (tries >= 4) {
            if (grace) clearInterval(grace);
            grace = null;
            fallback = false;
            rebuilt += 1;
            onsettled?.('recovered', now >= 0 ? now : undefined);
          }
        }, 1000);
        return;
      }
      // Still the same picture: watch it again from where it stands.
      awayAt = -1;
      lastTime = -1;
      watch();
    };

    const video = player.querySelector('video');
    video?.addEventListener('playing', onplaying);
    video?.addEventListener('pause', onpause);
    document.addEventListener('visibilitychange', onVisibility);

    return () => {
      if (armStartFor === armStart) armStartFor = null;
      if (timer) clearTimeout(timer);
      if (watching) clearInterval(watching);
      if (grace) clearInterval(grace);
      video?.removeEventListener('playing', onplaying);
      video?.removeEventListener('pause', onpause);
      document.removeEventListener('visibilitychange', onVisibility);
      player.remove();
    };
  });

  // Leaving waking opens the start window: the arm is taken here rather
  // than in the setup effect so a waiting flip never rebuilds the player.
  $effect(() => {
    if (waiting || !armStartFor) return;
    const arm = armStartFor;
    armStartFor = null;
    arm();
  });

  // The fallback is a picture like any other: a stalled one must not stand
  // there, and a session that died underneath must be asked for again when
  // the tab comes back - HLS re-attaches on its own, but nothing re-mints.
  $effect(() => {
    const video = picture;
    if (!fallback || !video) return;

    let watching: ReturnType<typeof setInterval> | null = null;
    let lastTime = -1;
    let paused = false;
    let started = false;
    let awayAt = -1;
    let grace: ReturnType<typeof setInterval> | null = null;

    const tick = () => {
      if (paused) {
        // A deliberate pause is not a stall here either.
        lastTime = video.currentTime;
        return;
      }
      if (video.currentTime <= lastTime) {
        if (watching) clearInterval(watching);
        watching = null;
        onsettled?.('frozen', video.currentTime);
        return;
      }
      lastTime = video.currentTime;
    };
    const watch = () => {
      if (watching) return;
      watching = setInterval(tick, WATCH_EVERY_MS);
    };
    const onplaying = () => {
      started = true;
      paused = false;
      watch();
    };
    const onpause = () => {
      paused = true;
    };
    const onVisibility = () => {
      if (document.hidden) {
        awayAt = started ? video.currentTime : -1;
        if (watching) clearInterval(watching);
        watching = null;
        return;
      }
      const here = video.currentTime;
      if (!started || (awayAt >= 0 && here <= awayAt)) {
        // The same beat as the stage player: a suspended tab's clock
        // resumes on its own and nothing should reset.
        if (grace) return;
        let tries = 0;
        grace = setInterval(() => {
          tries += 1;
          if (video.currentTime > here) {
            if (grace) clearInterval(grace);
            grace = null;
            awayAt = -1;
            lastTime = -1;
            watch();
            return;
          }
          if (tries >= 4) {
            if (grace) clearInterval(grace);
            grace = null;
            onsettled?.('recovered', here >= 0 ? here : undefined);
          }
        }, 1000);
        return;
      }
      awayAt = -1;
      lastTime = -1;
      watch();
    };

    video.addEventListener('playing', onplaying);
    video.addEventListener('pause', onpause);
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      if (watching) clearInterval(watching);
      if (grace) clearInterval(grace);
      video.removeEventListener('playing', onplaying);
      video.removeEventListener('pause', onpause);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  });

  // The video element arrives with the connection, so the default mute has
  // to be applied to whatever is on the stage - once per element: from
  // there the player's own control owns the volume, and a native unmute
  // is never reverted under the person's finger.
  $effect(() => {
    const want = muted;
    const timer = setInterval(() => {
      const video = host?.querySelector('video') as
        | (HTMLVideoElement & { nazarDefault?: boolean })
        | null;
      if (!video || video.nazarDefault) return;
      video.muted = want;
      video.volume = want ? 0 : 1;
      video.nazarDefault = true;
    }, 500);
    return () => clearInterval(timer);
  });

  // The vendored player carries HLS where the browser cannot: it needs the
  // element and its media attached by hand.
  $effect(() => {
    const element = picture;
    const name = stream;
    if (!fallback || native || !element || !name) return;
    const Hls = hlsJs();
    if (!Hls) return;
    const hls = new Hls();
    hls.loadSource(`${serverBase()}/go2rtc/api/stream.m3u8?src=${encodeURIComponent(name)}`);
    hls.attachMedia(element);
    return () => hls.destroy();
  });

  // A new attempt (or a fallback reversal) starts the socket over.
  $effect(() => {
    void attempt;
    fallback = false;
  });
</script>

<div class="stage" class:gone={fallback} bind:this={host} data-testid="player"></div>

{#if fallback}
  <video
    class="hls"
    data-testid="hls"
    bind:this={picture}
    src={native ? `${serverBase()}/go2rtc/api/stream.m3u8?src=${encodeURIComponent(stream)}` : undefined}
    controls
    autoplay
    playsinline
    muted={muted}
  ></video>
{/if}

<style>
  .stage {
    aspect-ratio: 16 / 9;
    border-radius: 8px;
    overflow: hidden;
    background: #000;
  }

  /* The fallback replaces the picture: a dead WebRTC stage left visible
     under it is two dead players on the screen at once. */
  .stage.gone {
    display: none;
  }

  .stage :global(video-rtc),
  .stage :global(video-rtc video) {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: contain;
  }

  .hls {
    display: block;
    width: 100%;
    aspect-ratio: 16 / 9;
    border-radius: 8px;
    background: #000;
  }
</style>
