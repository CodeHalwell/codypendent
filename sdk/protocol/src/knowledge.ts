/** Schema-owned knowledge projections, reached through the public payload union. */
import type { Payload } from "./envelope.js";
export type KnowledgePage = Extract<Payload, { type: "KnowledgePage" }>["page"];
export type KnowledgeSection = KnowledgePage["section"];
export type SkillCard = NonNullable<KnowledgePage["skills"]>[number];
export type MemoryCard = NonNullable<KnowledgePage["memories"]>[number];
export type LearningCard = NonNullable<KnowledgePage["learnings"]>[number];
export type DocCard = NonNullable<KnowledgePage["documents"]>[number];
export type DocBlockView = DocCard["blocks"][number];
export type DocSuggestionView = DocCard["suggestions"][number];
