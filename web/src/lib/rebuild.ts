/** The stalled picture's auto-rebuild: a dead picture is rebuilt and
 * re-asked on its own, waiting one backoff step longer each time - a
 * picture that settles resets the ladder. The manual escape ("Try again")
 * stays immediate. */

const STEPS_MS = [1_000, 2_000, 5_000, 10_000, 30_000];

export function rebuildSchedule(onFire: (kind: 'start' | 'frozen') => void) {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let step = 0;
  let pending: 'start' | 'frozen' = 'start';
  return {
    /** Arm the next recovery; a pending one is already the answer. */
    schedule(kind: 'start' | 'frozen') {
      if (timer) return;
      pending = kind;
      const wait = STEPS_MS[Math.min(step, STEPS_MS.length - 1)];
      step += 1;
      timer = setTimeout(() => {
        timer = null;
        onFire(pending);
      }, wait);
    },
    /** The picture settled (or the person acted): start the ladder over. */
    settled() {
      step = 0;
      if (timer) clearTimeout(timer);
      timer = null;
    },
    /** The component is going away. */
    cancel() {
      if (timer) clearTimeout(timer);
      timer = null;
    },
  };
}
