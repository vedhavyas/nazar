<script lang="ts">
  /** The picture: go2rtc's WebRTC signalling, with HLS as the fallback for
   * a network where the socket cannot negotiate (design §5). */
  interface VideoRtcElement extends HTMLElement {
    src: string;
    mode: string;
    media: string;
  }

  let {
    stream,
    muted = true,
    attempt = 0,
    onsettled,
  }: {
    stream: string;
    muted?: boolean;
    /** Bump this to start the picture over from the WebRTC socket. */
    attempt?: number;
    /** Told when the picture was not there: 'start' for a socket that never
     * negotiated (and no HLS to fall back to), 'frozen' for one that
     * stopped, 'hls' for a fallback that took over, 'recovered' for a
     * picture that was started again on coming back to the tab. */
    onsettled?: (state: 'start' | 'frozen' | 'hls' | 'recovered') => void;
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
    player.src = `/go2rtc/api/ws?src=${encodeURIComponent(name)}`;
    stage.appendChild(player);

    // WebRTC fails quietly: a blocked ICE path leaves a black frame, and a
    // dead stream leaves the last one. No first frame inside the window,
    // then no advancing clock, are the two signals a person can act on.
    const stall = (state: 'start' | 'frozen') => {
      void engageFallback().then((taken) => {
        onsettled?.(taken ? 'hls' : state);
      });
    };
    let timer: ReturnType<typeof setTimeout> | null = setTimeout(
      () => stall('start'),
      START_WINDOW_MS,
    );
    let watching: ReturnType<typeof setInterval> | null = null;
    let lastTime = -1;
    let playing = false;
    /** Where the clock stood when the tab was hidden, if it had started. */
    let awayAt = -1;

    const tick = () => {
      const video = player.querySelector('video');
      if (!video) return;
      if (video.currentTime <= lastTime) {
        if (watching) clearInterval(watching);
        watching = null;
        stall('frozen');
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
      if (timer) clearTimeout(timer);
      timer = null;
      watch();
    };

    // A background tab is not a stalled picture: Chromium throttles it and
    // may drop the peer behind its back, so the watch stands down while it
    // cannot see, and the picture is started again on return when the clock
    // did not follow us back - a dropped peer never re-attaches on its own.
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
        fallback = false;
        rebuilt += 1;
        onsettled?.('recovered');
        return;
      }
      // Still the same picture: watch it again from where it stands.
      awayAt = -1;
      lastTime = -1;
      watch();
    };

    const video = player.querySelector('video');
    video?.addEventListener('playing', onplaying);
    document.addEventListener('visibilitychange', onVisibility);

    return () => {
      if (timer) clearTimeout(timer);
      if (watching) clearInterval(watching);
      video?.removeEventListener('playing', onplaying);
      document.removeEventListener('visibilitychange', onVisibility);
      player.remove();
    };
  });

  // The video element arrives with the connection, so the mute the caller
  // set has to be re-applied to whatever is on the stage.
  $effect(() => {
    const timer = setInterval(() => {
      const video = host?.querySelector('video');
      if (!video) return;
      if (video.muted !== muted) {
        video.muted = muted;
      }
      video.volume = muted ? 0 : 1;
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
    hls.loadSource(`/go2rtc/api/stream.m3u8?src=${encodeURIComponent(name)}`);
    hls.attachMedia(element);
    return () => hls.destroy();
  });

  // A new attempt (or a fallback reversal) starts the socket over.
  $effect(() => {
    void attempt;
    fallback = false;
  });
</script>

<div class="stage" bind:this={host} data-testid="player"></div>

{#if fallback}
  <video
    class="hls"
    data-testid="hls"
    bind:this={picture}
    src={native ? `/go2rtc/api/stream.m3u8?src=${encodeURIComponent(stream)}` : undefined}
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
