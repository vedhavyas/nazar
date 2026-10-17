/** The picture's liveness, one level above the player: a tile watches the
 * element's own state, not the player's events, so a picture that never
 * paints - or a house that keeps remounting its players - cannot stand
 * there dead and silent. The player's own windows die with the player;
 * this one lives with the tile.
 *
 * The window outlasts the player's own start window (12s) so the WebRTC
 * socket always gets its chance to reach for the fallback first; this
 * watch only speaks when nothing - socket or fallback - ever painted. */

const PICTURE_WINDOW_MS = 20_000;
const CHECK_EVERY_MS = 2_500;

/** A picture is here when the element holds data or its clock has moved;
 * a stream being torn down sits at readyState 0 and clock 0. */
function holdsPicture(node: HTMLElement | null): boolean {
  const video = node?.querySelector('video') as HTMLVideoElement | null;
  return !!video && (video.readyState >= 2 || video.currentTime > 0);
}

export function pictureWatch(
  element: () => HTMLElement | null,
  onVerdict: (state: 'start' | 'painted') => void,
  blocked: () => boolean = () => false,
) {
  let missingSince: number | null = Date.now();
  let noted = false;
  let here = false;
  const timer = setInterval(() => {
    if (blocked()) {
      // The server is still waking: the window has nothing to run against.
      missingSince = null;
      noted = false;
      return;
    }
    if (holdsPicture(element())) {
      missingSince = null;
      noted = false;
      if (!here) {
        here = true;
        onVerdict('painted');
      }
      return;
    }
    here = false;
    missingSince ??= Date.now();
    if (!noted && Date.now() - missingSince >= PICTURE_WINDOW_MS) {
      noted = true;
      onVerdict('start');
    }
  }, CHECK_EVERY_MS);
  return {
    cancel() {
      clearInterval(timer);
    },
  };
}
