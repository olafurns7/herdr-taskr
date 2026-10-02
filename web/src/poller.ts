// The state poll: one request in flight at a time, one timer, paused while
// the page is hidden, and never an older response over a newer one.

export interface PollerDeps<T> {
  fetch: () => Promise<T>;
  apply: (v: T) => void;
  fail: (e: unknown) => void;
  hidden: () => boolean;
  setTimeout: (fn: () => void, ms: number) => number;
  clearTimeout: (id: number) => void;
  every: number; // ms between the end of one poll and the start of the next
  /** at orders responses (server time, ms): one older than the last shown is dropped. */
  at: (v: T) => number;
}

export interface Poller {
  /** wake polls now, unless a poll is in flight (it is fresh enough) or the page is hidden. */
  wake: () => void;
  /** visibility pauses the timer on hide and polls on show. */
  visibility: () => void;
  stop: () => void;
}

export function poller<T>(d: PollerDeps<T>): Poller {
  let timer: number | null = null;
  let inFlight = false;
  let stopped = false;
  let issued = 0; // sequence of the last request sent
  let applied = 0; // sequence of the last response shown
  let appliedAt = -Infinity; // its server time

  const disarm = () => {
    if (timer !== null) d.clearTimeout(timer);
    timer = null;
  };
  const arm = () => {
    disarm();
    if (!stopped && !d.hidden()) timer = d.setTimeout(wake, d.every);
  };
  function wake() {
    if (stopped || inFlight || d.hidden()) return;
    disarm();
    inFlight = true;
    const seq = ++issued;
    d.fetch().then(
      (v) => {
        const at = d.at(v);
        if (!stopped && seq > applied && !(at < appliedAt)) {
          applied = seq;
          appliedAt = at;
          d.apply(v);
        }
      },
      (e) => {
        if (!stopped && seq > applied) d.fail(e);
      },
    ).finally(() => {
      inFlight = false;
      arm();
    });
  }
  wake();
  return {
    wake,
    visibility: () => (d.hidden() ? disarm() : wake()),
    stop: () => {
      stopped = true;
      disarm();
    },
  };
}
