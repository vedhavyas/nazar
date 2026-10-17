/** Per-camera audio: every video starts muted, the speaker toggle is the
 * gesture that unmutes one camera, and the choice is remembered under one
 * key across surfaces. */

export function audioKey(id: string): string {
  return `nazar.audio.${id}`;
}

export function audioOn(id: string): boolean {
  try {
    return localStorage.getItem(audioKey(id)) === '1';
  } catch {
    return false;
  }
}

export function rememberAudio(id: string, on: boolean): void {
  try {
    localStorage.setItem(audioKey(id), on ? '1' : '0');
  } catch {
    // Not worth an error: the toggle still works for this visit.
  }
}
