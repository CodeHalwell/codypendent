import type { SessionSummary } from "@codypendent/protocol";
import type { KnowledgeTransport, SkillCard } from "../../src/components/knowledgeTransport.js";
import type { DesktopTransport } from "../../src/transport.js";

export class KnowledgeSessionFixture {
  selected = "workspace";
  skillsPending: Promise<SkillCard[]> | null = null;
  correctionPending: Promise<string> | null = null;
  readonly reads: string[] = [];
  readonly sessions: SessionSummary[] = ["A", "B"].map((name) => ({
    session_id: name,
    title: name,
    state: "open",
    created_at: "2026-10-26T00:00:00Z",
    updated_at: "2026-10-26T00:00:00Z",
  }));
  readonly transport = {
    connect: async () => ({
      socket_path: "/tmp/knowledge-test.sock",
      protocol_version: "1.11",
      daemon_version: "0.14.0",
      daemon_instance: "knowledge-test",
      build_id: "knowledge-test",
    }),
    disconnect: async () => undefined,
    socketPath: async () => "/tmp/knowledge-test.sock",
    listSessions: async () => this.sessions,
    listInbox: async () => ({ items: [], next_cursor: null }),
    attachSession: async (sessionId: string) => { this.selected = sessionId; },
  } as unknown as DesktopTransport;
  readonly knowledge = {
    listSkills: async () => {
      this.reads.push(`skills:${this.selected}`);
      const pending = this.skillsPending;
      this.skillsPending = null;
      return pending ?? [this.skill()];
    },
    listMemories: async () => [{
      id: this.selected, statement: `Memory ${this.selected}`, class: "semantic",
      scope: "repository", revision: "r1", observed: "2026-10-26", confidence: 1, source: "test",
    }],
    correctMemory: async () => this.correctionPending ?? "Memory corrected",
    listLearnings: async () => [{
      id: this.selected, statement: `Learning ${this.selected}`, kind: "fact", state: "active",
      scope: "repository", provenance: "test", confidence: 1, pinned: false, revision: 1,
    }],
    listDocuments: async () => [{
      document_id: this.selected, title: `Document ${this.selected}`, scope: "repository",
      status: "active", mode: "review", revision: "r1", blocks: [], suggestions: [],
    }],
  } as unknown as KnowledgeTransport;

  skill(): SkillCard {
    return {
      name: `Skill ${this.selected}`, kind: "skill", scope: "repository", trust: "first-party",
      status: "active", risk: "safe", description: "Scoped skill", permissions: [],
    };
  }
}
