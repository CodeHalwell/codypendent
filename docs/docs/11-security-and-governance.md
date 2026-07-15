# Security and Governance

## Threat model

Codypendent executes model-proposed actions against valuable developer environments. Threats include:

- prompt injection from repositories, documents, websites, tool output, or plugins;
- malicious or compromised skills and MCP servers;
- secret exfiltration;
- unsafe shell execution;
- path traversal and symlink attacks;
- privilege escalation;
- dependency and plugin supply-chain attacks;
- confused-deputy actions through GitHub or cloud integrations;
- replayed or duplicated commands;
- cross-scope memory leakage;
- malicious model output;
- compromised local client;
- poisoned code-index or knowledge entries.

## Policy flow

```text
model or user proposes action
        ↓
schema validation
        ↓
source/trust classification
        ↓
policy evaluation
        ↓
capability grant construction
        ↓
approval if required
        ↓
sandbox execution
        ↓
output sanitization
        ↓
trace and outcome
```

## Capability model

```rust
pub enum Capability {
    FileRead(PathScope),
    FileWrite(PathScope),
    CommandExecute(CommandScope),
    NetworkConnect(NetworkScope),
    SecretUse(SecretId),
    GitCommit(RepositoryId),
    GitPush(RemoteScope),
    GitHubWrite(GitHubScope),
    ProcessSpawn(ProcessScope),
}
```

Capabilities are invocation-scoped and time-limited.

## Scope hierarchy

```text
System
→ Organization
→ User
→ Workspace
→ Repository
→ Branch
→ Session
→ Task
```

Preferences merge from broad to narrow. Security restrictions do not.

Rules:

- deny wins over allow;
- lower scopes cannot weaken higher-authority restrictions;
- temporary grants expire;
- secret access is explicit;
- policy decisions are logged.

## Data classification

```rust
pub enum DataClassification {
    Public,
    Internal,
    Confidential,
    Restricted,
    Secret,
}
```

Model policy maps classifications to eligible providers. Secrets are never placed into ordinary model context.

## Secrets

Use the operating-system keychain for the personal product. The database stores secret identifiers and metadata, not plaintext.

A tool may receive a secret through a brokered channel without the model seeing it.

## Command security

The shell tool should support structured command policies:

```rust
pub struct CommandRequest {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: Vec<EnvironmentBinding>,
    pub timeout: Duration,
}
```

Avoid executing a single unparsed shell string unless a user explicitly approves shell interpretation.

Validate:

- canonical paths;
- symlink boundaries;
- executable allowlists;
- environment variables;
- output limits;
- timeout;
- child process count;
- network policy.

## Plugin security

### WASM plugins

Run with explicit imported capabilities, resource limits, and no ambient host access.

### Native plugins

Run under platform-specific OS isolation with:

- restricted token/user;
- constrained filesystem;
- network allowlist;
- CPU/memory/process limits;
- clean environment;
- no inherited descriptors;
- signed manifest and checksum.

MCP is a protocol, not a trust guarantee.

## Prompt injection handling

Tool, skill, document, code, and web content is labeled by origin. Retrieved content cannot directly grant permissions or alter system policy.

The context compiler separates:

- system policy;
- user instruction;
- trusted repository policy;
- untrusted retrieved content;
- tool observations.

Model output proposing a privileged action still passes through policy and approval.

## Worktree safety

Before deletion:

- reconcile with Git;
- detect unmerged commits;
- preserve or export patches;
- stop owned processes;
- release leases;
- require override for destructive cleanup.

Repository-specific checks such as git-crypt, SOPS, signed commits, or secret scanning are policy plugins rather than universal core assumptions.

## Audit

Security-relevant records include:

- policy input and decision;
- requested and granted capabilities;
- approver;
- tool source and version;
- model and prompt-policy version;
- external target;
- outcome;
- artifact hashes.

Sensitive payloads may be redacted while retaining structural audit data.

## Responsible learning

Failed or malicious traces may be used for evaluation, but must be sanitized before becoming examples. A model must not learn to bypass a policy merely because a user once granted an exception.

## Autonomy, search, and runner policy

Autonomy is independent from individual capabilities. Bounded autopilot may still have network disabled and GitHub writes denied.

Web retrieval modes are visible: disabled, cached only, allowlisted, live with approval or live under a task-specific grant.

Moving work to a LAN or hosted runner changes data residency, secrets availability and cost and may require approval.
