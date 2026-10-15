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
     * stopped, 'hls' for a fallback that took over. */
    onsettled?: (state: 'start' | 'frozen' | 'hls') => void;
  } = $props();

  /** How long a first frame may take before WebRTC is given up on. */
  const START_WINDOW_MS = 12_000;
  /** How often an advancing clock is looked for once the picture starts. */
  const WATCH_EVERY_MS = 5_000;

  let host = $state<HTMLDivElement | null>(null);
  let fallback = $state(false);

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
    const timer = setTimeout(() => {
      if (playsHls()) {
        fallback = true;
        onsettled?.('hls');
        return;
      }
      onsettled?.('start');
    }, START_WINDOW_MS);
    let watching: ReturnType<typeof setInterval> | null = null;
    let lastTime = -1;
    const onplaying = () => {
      clearTimeout(timer);
      if (watching) return;
      watching = setInterval(() => {
        const video = player.querySelector('video');
        if (!video) return;
        if (video.currentTime <= lastTime) {
          if (playsHls()) {
            fallback = true;
            onsettled?.('hls');
            return;
          }
          onsettled?.('frozen');
          return;
        }
        lastTime = video.currentTime;
      }, WATCH_EVERY_MS);
    };
    const video = player.querySelector('video');
    video?.addEventListener('playing', onplaying);

    return () => {
      clearTimeout(timer);
      if (watching) clearInterval(watching);
      video?.removeEventListener('playing', onplaying);
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
    src="/go2rtc/api/stream.m3u8?src={encodeURIComponent(stream)}"
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
