# Codypendent Project Scaffold

This scaffold keeps the repository lightweight while setting up clear implementation lanes.

## 1) Repository Layout

```text
.
├── README.md
├── docs/
│   ├── PROJECT_SCAFFOLD.md
│   ├── TIMELINE.md
│   ├── architecture/
│   │   ├── backend-daemon.md
│   │   ├── protocol.md
│   │   └── policy-and-scope.md
│   ├── product/
│   │   ├── positioning.md
│   │   ├── terminology.md
│   │   └── launch-checklist.md
│   └── workflows/
│       ├── ci-fix-flow.md
│       ├── pr-review-flow.md
│       └── release-flow.md
├── crates/
│   ├── codypendent-cli/
│   ├── codypendent-daemon/
│   ├── codypendent-protocol/
│   ├── codypendent-skills/
│   └── codypendent-fabric/
└── examples/
    ├── skills/
    └── sessions/
```

## 2) Naming Conventions

- Binaries: `codypendent`, `codypendentd`
- Config directory: `.codypendent/`
- Rust crates: `codypendent-*`
- Protocol docs/types: `Codypendent Protocol`
- Knowledge and memory system docs/types: `Codypendent Fabric`

## 3) Initial API Surface (CLI)

```bash
codypendent
codypendent open .
codypendent agent run fix-ci
codypendent skills edit rust-reviewer
codypendent daemon status
```

## 4) Product Vocabulary (Optional Theme Layer)

Keep the relationship-comedy language optional and off the core API.

- Memory -> Baggage
- Permissions -> Boundaries
- Plugins -> Attachments
- Multi-agent session -> Group Therapy
- Model router -> Couples Counsellor
- Compaction -> Processing
- Failed runs -> Past Mistakes
- Workspaces -> Relationships
- Dependencies -> Emotional Dependencies
- Context window -> Attention Span
- Approval request -> Seeking Validation
- Agent handoff -> Seeing Other Models
- Sandbox -> Personal Space

## 5) Immediate Next Scaffolding Steps

1. Create Rust workspace with `codypendent-cli`, `codypendent-daemon`, and `codypendent-protocol`.
2. Add shared config schema for `.codypendent/`.
3. Add daemon health/status command and a no-op session lifecycle.
4. Add one reference skill package under `examples/skills/`.
5. Add baseline CI (format, lint, test) once code is introduced.
