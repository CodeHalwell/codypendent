import type { Catchup, SessionProjection } from "./catchup.js";
import type { SessionEvent } from "./generated/events.js";

export interface SessionStoreSnapshot {
  readonly ready: boolean;
  readonly cursor: number;
  readonly projection?: SessionProjection;
  /**
   * The retained tail of the session's events — at most
   * {@link MAX_RETAINED_EVENTS}, newest last.
   *
   * This is a window, not the history. `cursor` remains the true watermark and
   * {@link SessionStoreSnapshot.droppedEvents} counts what fell off the front,
   * so a consumer can always tell that it is looking at a tail.
   */
  readonly events: readonly SessionEvent[];
  /** How many events have been dropped off the front of `events`. */
  readonly droppedEvents: number;
}

/**
 * Most events a snapshot retains.
 *
 * The store appended every event forever, and did it by copying the whole
 * array each time — quadratic in the length of the session, on the hot path of
 * a live stream. A long run therefore paid steadily more per event for a
 * history nothing was reading. The cap fixes both halves at once: memory stops
 * growing, and each append copies at most this many entries instead of all of
 * them.
 *
 * Deep enough to hold far more than any transcript view asks for, so the tail
 * is a real safety valve rather than a routine truncation.
 */
export const MAX_RETAINED_EVENTS = 2_000;

export type SessionStoreListener = () => void;

export class SessionSequenceGapError extends Error {
  readonly expected: number;
  readonly received: number;

  constructor(expected: number, received: number) {
    super(`session event gap: expected sequence ${expected}, received ${received}`);
    this.name = "SessionSequenceGapError";
    this.expected = expected;
    this.received = received;
  }
}

const EMPTY_SNAPSHOT: SessionStoreSnapshot = {
  ready: false,
  cursor: 0,
  events: [],
  droppedEvents: 0,
};

/** The newest {@link MAX_RETAINED_EVENTS} of `events`, and how many were cut. */
function retainTail(events: readonly SessionEvent[]): {
  readonly kept: SessionEvent[];
  readonly dropped: number;
} {
  if (events.length <= MAX_RETAINED_EVENTS) {
    return { kept: [...events], dropped: 0 };
  }
  const dropped = events.length - MAX_RETAINED_EVENTS;
  return { kept: events.slice(dropped), dropped };
}

/**
 * Host-neutral attach/catch-up/live event store.
 *
 * Live events received before the attach catch-up are held back so consumers
 * never observe a live-ready state with missing history.
 */
export class SessionStore {
  private snapshot: SessionStoreSnapshot = EMPTY_SNAPSHOT;
  private buffered = new Map<number, SessionEvent>();
  private catchingUp = false;
  private readonly listeners = new Set<SessionStoreListener>();

  getSnapshot = (): SessionStoreSnapshot => this.snapshot;

  subscribe = (listener: SessionStoreListener): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  /** Hold new wire events until the next catch-up validates the resume baseline. */
  beginCatchup(): void {
    this.catchingUp = true;
    this.buffered.clear();
  }

  applyEvent(event: SessionEvent): void {
    if (!Number.isSafeInteger(event.sequence) || event.sequence < 1) {
      throw new Error("invalid session event sequence");
    }
    if (!this.snapshot.ready || this.catchingUp) {
      if (!this.buffered.has(event.sequence) && this.buffered.size >= MAX_RETAINED_EVENTS) {
        throw new Error("session catch-up buffer is full");
      }
      this.buffered.set(event.sequence, event);
      return;
    }

    if (event.sequence <= this.snapshot.cursor) return;
    this.assertNext(this.snapshot.cursor, event.sequence);
    const { kept, dropped } = retainTail([...this.snapshot.events, event]);
    this.snapshot = {
      ...this.snapshot,
      cursor: event.sequence,
      events: kept,
      droppedEvents: this.snapshot.droppedEvents + dropped,
    };
    this.emit();
  }

  applyCatchup(catchup: Catchup): void {
    if (catchup.type === "Unknown") {
      throw new Error("cannot initialize a session store from an unknown catch-up");
    }

    let cursor: number;
    let projection: SessionProjection | undefined;
    let events: SessionEvent[];

    if (catchup.type === "Snapshot") {
      if (catchup.projection.last_sequence !== catchup.through) {
        throw new Error(
          `snapshot watermark mismatch: projection is at ${catchup.projection.last_sequence}, catch-up is through ${catchup.through}`,
        );
      }
      cursor = catchup.through;
      projection = catchup.projection;
      events = [];
    } else {
      const expectedBaseline = catchup.from - 1;
      if (this.snapshot.ready && this.snapshot.cursor < expectedBaseline) {
        throw new SessionSequenceGapError(this.snapshot.cursor + 1, catchup.from);
      }
      cursor = this.snapshot.ready ? this.snapshot.cursor : expectedBaseline;
      projection = this.snapshot.projection;
      events = this.snapshot.ready ? [...this.snapshot.events] : [];
      for (const event of catchup.events) {
        if (event.sequence <= cursor) continue;
        this.assertNext(cursor, event.sequence);
        events.push(event);
        cursor = event.sequence;
      }
      if (cursor !== catchup.through) {
        throw new Error(`catch-up watermark mismatch: events end at ${cursor}, catch-up is through ${catchup.through}`);
      }
    }

    for (const event of [...this.buffered.values()].sort((a, b) => a.sequence - b.sequence)) {
      if (event.sequence <= cursor) continue;
      this.assertNext(cursor, event.sequence);
      events.push(event);
      cursor = event.sequence;
    }

    this.buffered.clear();
    this.catchingUp = false;
    const { kept, dropped } = retainTail(events);
    // A Snapshot catch-up restarts the history from a projection, so its
    // dropped count restarts with it; an Events catch-up extends what is
    // already held and carries the running total forward.
    const carried = catchup.type === "Snapshot" ? 0 : this.snapshot.droppedEvents;
    this.snapshot = {
      ready: true,
      cursor,
      ...(projection === undefined ? {} : { projection }),
      events: kept,
      droppedEvents: carried + dropped,
    };
    this.emit();
  }

  reset(): void {
    this.buffered.clear();
    this.catchingUp = false;
    this.snapshot = EMPTY_SNAPSHOT;
    this.emit();
  }

  private assertNext(cursor: number, sequence: number): void {
    const expected = cursor + 1;
    if (sequence > expected) throw new SessionSequenceGapError(expected, sequence);
  }

  private emit(): void {
    for (const listener of this.listeners) listener();
  }
}
