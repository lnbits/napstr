/**
 * Temporary cover-art telemetry for the in-app debug panel.
 *
 * Delete this file, `CoverDebug.svelte`, the `recordCoverEvent` calls in
 * `artwork.ts`, and the `COVER_DEBUG` block in `App.svelte` once artwork is
 * trusted. Nothing else depends on it.
 */
export type CoverEvent = {
  /** Monotonic, so the log can be keyed without depending on its text. */
  id: number;
  at: number;
  kind: 'request' | 'answer' | 'error' | 'cached' | 'skip' | 'refresh';
  detail: string;
};

const EVENT_LIMIT = 150;
const events: CoverEvent[] = [];
const listeners = new Set<() => void>();
let sequence = 0;

export function recordCoverEvent(kind: CoverEvent['kind'], detail: string) {
  events.unshift({ id: sequence, at: Date.now(), kind, detail });
  sequence += 1;
  if (events.length > EVENT_LIMIT) events.length = EVENT_LIMIT;
  for (const listener of listeners) listener();
}

export function coverEvents(): CoverEvent[] {
  return events;
}

export function clearCoverEvents() {
  events.length = 0;
  for (const listener of listeners) listener();
}

export function subscribeCoverEvents(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}
