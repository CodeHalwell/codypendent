/** Stable public facade for schema-generated event contracts. */

export type { Actor, EventBody, SessionEvent, UsageCoverage } from "./generated/events.js";

import type { EventBody } from "./generated/events.js";

/** Every `EventBody` tag this build knows. Kept exhaustive by `tags.ts`. */
export type EventBodyTag = EventBody["type"];
