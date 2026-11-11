import type { ThreadTotals } from './provider.ts';

/** Bursts of `changes` read the account's totals once. */
export const THREAD_REFRESH_MS = 400;

/** A filtered list shorter than this reads on, the view being unfilled. */
export const THREAD_FILL_TARGET = 12;
/** Pages read on per user action, so a rare filter never walks the whole account. */
export const THREAD_FILL_PAGES = 4;

export function shouldFillThreads(list: { shown: number; next: string | null; busy: boolean; budget: number }): boolean {
  return list.next !== null && !list.busy && list.budget > 0 && list.shown < THREAD_FILL_TARGET;
}

/** Calls `run` once, `delayMs` after the last `call` of a burst. */
export function coalesced(run: () => void, delayMs: number): { call: () => void; cancel: () => void } {
  let timer: ReturnType<typeof setTimeout> | null = null;
  const cancel = () => {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  };
  return {
    call: () => {
      cancel();
      timer = setTimeout(() => {
        timer = null;
        run();
      }, delayMs);
    },
    cancel,
  };
}

/**
 * The account's unread thread counts, shared by the screens that show them: the
 * room list and the threads screen under it read once per burst, and a read
 * that changes arrive during is followed by one more.
 */
export class ThreadTotalsStore {
  private totals: ThreadTotals | null = null;
  private readonly listeners = new Set<() => void>();
  private readonly read: () => Promise<ThreadTotals>;
  private readonly changes: (listener: () => void) => () => void;
  private readonly pace: { call: () => void; cancel: () => void };
  private release: (() => void) | null = null;
  private reading = false;
  private again = false;

  constructor(read: () => Promise<ThreadTotals>, changes: (listener: () => void) => () => void, delayMs = THREAD_REFRESH_MS) {
    this.read = read;
    this.changes = changes;
    this.pace = coalesced(() => void this.fetch(), delayMs);
  }

  current = (): ThreadTotals | null => this.totals;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    this.release ??= this.changes(this.pace.call);
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size > 0) return;
      this.release?.();
      this.release = null;
      this.pace.cancel();
    };
  };

  schedule(): void {
    this.pace.call();
  }

  private async fetch(): Promise<void> {
    if (this.reading) {
      this.again = true;
      return;
    }
    this.reading = true;
    try {
      this.totals = await this.read();
      for (const listener of this.listeners) listener();
    } catch {
      // The next change or connection reads again; the count on screen stays.
    }
    this.reading = false;
    if (this.again) {
      this.again = false;
      void this.fetch();
    }
  }
}

/**
 * When an open thread is read: at most once per reply seen, a burst sharing one
 * call. A pending call covers what arrives until it fires, since the server
 * reads up to the moment it is sent. A failed call is sent again at the next
 * `note`.
 */
export class ThreadReadPacer {
  private readonly send: () => Promise<void>;
  private readonly delayMs: number;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private wanted: string | null = null;
  private sent: string | null = null;

  constructor(send: () => Promise<void>, delayMs = THREAD_REFRESH_MS) {
    this.send = send;
    this.delayMs = delayMs;
  }

  note(latest: string): void {
    this.wanted = latest;
    if (latest === this.sent || this.timer !== null) return;
    this.timer = setTimeout(() => this.fire(), this.delayMs);
  }

  flush(): void {
    if (this.timer === null) return;
    clearTimeout(this.timer);
    this.fire();
  }

  private fire(): void {
    this.timer = null;
    const covered = this.wanted;
    if (covered === this.sent) return;
    this.sent = covered;
    this.send().catch(() => {
      if (this.sent === covered) this.sent = null;
    });
  }
}
