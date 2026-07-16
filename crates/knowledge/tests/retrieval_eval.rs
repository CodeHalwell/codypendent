//! STEP 2.3 evaluation gate — the headline Phase-2 exit criterion.
//!
//! Top-k hybrid retrieval must beat full-tool injection: on a 30-case labelled
//! set it achieves **mean recall@8 ≥ 0.8** while **excluding 100 %** of the
//! forbidden (destructive) decoys, and the disclosed cards fit a context budget
//! that dumping every item's full definition blows past. The pool is seeded with
//! the five built-in tools, the `rust.fix-ci` reference skill, and 20 synthetic
//! decoys (three of them High-risk and destructive) so retrieval has to both
//! rank the right behaviours and filter the dangerous ones.

use std::collections::HashSet;
use std::path::PathBuf;

use codypendent_knowledge::retrieval::{
    retrieve, HashingEmbedder, RetrievalConfig, RetrievalIndexes, RetrievalQuery,
};
use codypendent_knowledge::types::{
    CapabilityRequest, Provenance, RegistryItem, RegistryItemKind, RegistryStatus, RiskClass,
    Scope, TrustMetadata, TrustTier, UsageExample, Version,
};
use codypendent_knowledge::{db, register_builtins, Registry};
use codypendent_protocol::{RegistryItemId, RepositoryId};
use serde::Deserialize;

/// One labelled case from `fixtures/retrieval-eval.jsonl`. Ids are item **names**
/// (the stable identity), not UUIDs.
#[derive(Debug, Deserialize)]
struct EvalCase {
    query: String,
    expected_tool_ids: Vec<String>,
    forbidden_ids: Vec<String>,
}

/// The three destructive decoys that must never be disclosed under the Medium
/// ceiling. They are High-risk, so the hard risk filter removes them.
const FORBIDDEN: [&str; 3] = [
    "decoy.database_drop_table",
    "decoy.aws_s3_delete",
    "decoy.k8s_delete_namespace",
];

/// The context budget, in estimated tokens. The disclosed cards must fit under
/// it; injecting every item's full definition must exceed it.
const BUDGET_TOKENS: usize = 1200;

async fn temp_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let tmp = tempfile::tempdir().unwrap();
    let pool = db::open(&tmp.path().join("codypendent.db")).await.unwrap();
    (tmp, pool)
}

/// Seed a temp pool with the built-ins, the `rust.fix-ci` reference skill (at
/// `repo`), and the 20 decoys, and return the listed authority.
async fn seed() -> (tempfile::TempDir, Vec<RegistryItem>, RepositoryId) {
    let (tmp, pool) = temp_pool().await;
    let registry = Registry::new();
    let repo = RepositoryId::new();

    register_builtins(&pool).await.unwrap();

    let fixci_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/skills/fix-ci");
    registry
        .register_package(&pool, &fixci_dir, Scope::Repository(repo))
        .await
        .unwrap();

    for decoy in decoys() {
        registry.upsert(&pool, &decoy).await.unwrap();
    }

    let items = registry.list(&pool).await.unwrap();
    // 5 built-ins + fix-ci + 20 decoys.
    assert_eq!(items.len(), 26, "unexpected seeded item count");
    (tmp, items, repo)
}

#[tokio::test]
async fn retrieval_beats_full_injection_recall_and_forbidden_exclusion() {
    let (_tmp, items, repo) = seed().await;

    // ---- Build the derived indexes over authority ---------------------------
    let indexes = RetrievalIndexes::build(&items, HashingEmbedder::new()).unwrap();

    // recall@8: disclose up to 8 tool cards (+ 1–3 skill cards).
    let config = RetrievalConfig {
        disclose_tools_max: 8,
        ..RetrievalConfig::default()
    };

    // ---- Run every fixture case ---------------------------------------------
    let cases = load_cases();
    assert_eq!(cases.len(), 30, "expected 30 eval cases");

    let mut recalls: Vec<(String, f32)> = Vec::new();
    let mut disclosed_token_peak = 0usize;
    let mut forbidden_ever_disclosed: Vec<String> = Vec::new();

    for case in &cases {
        let query = RetrievalQuery {
            text: case.query.clone(),
            visible_scopes: vec![Scope::System, Scope::Repository(repo)],
            risk_ceiling: RiskClass::Medium,
            min_trust: TrustTier::Untrusted,
            history: Vec::new(),
        };
        let result = retrieve(&items, &indexes, &query, &config).unwrap();

        let disclosed: HashSet<&str> = result
            .tools
            .iter()
            .chain(result.skills.iter())
            .map(|card| card.name.as_str())
            .collect();

        // recall@8 by name over the expected set.
        let hit = case
            .expected_tool_ids
            .iter()
            .filter(|name| disclosed.contains(name.as_str()))
            .count();
        let recall = hit as f32 / case.expected_tool_ids.len() as f32;
        recalls.push((case.query.clone(), recall));

        // Forbidden exclusion is HARD: no forbidden name, ever.
        for forbidden in &case.forbidden_ids {
            if disclosed.contains(forbidden.as_str()) {
                forbidden_ever_disclosed.push(format!("{forbidden} for query: {}", case.query));
            }
        }
        for forbidden in FORBIDDEN {
            if disclosed.contains(forbidden) {
                forbidden_ever_disclosed.push(format!("{forbidden} for query: {}", case.query));
            }
        }

        // Track the worst-case disclosed footprint for the budget assertion.
        let disclosed_tokens: usize = result
            .tools
            .iter()
            .chain(result.skills.iter())
            .map(|card| est_tokens(&format!("{} {}", card.name, card.summary)))
            .sum();
        disclosed_token_peak = disclosed_token_peak.max(disclosed_tokens);
    }

    // ---- Forbidden exclusion = 100 % ----------------------------------------
    assert!(
        forbidden_ever_disclosed.is_empty(),
        "forbidden items leaked into disclosure: {forbidden_ever_disclosed:?}"
    );

    // ---- Mean recall@8 ≥ 0.8 ------------------------------------------------
    let mean_recall = recalls.iter().map(|(_, r)| *r).sum::<f32>() / recalls.len() as f32;
    let mut worst = recalls.clone();
    worst.sort_by(|a, b| a.1.total_cmp(&b.1));
    eprintln!("mean recall@8 = {mean_recall:.4}");
    eprintln!("hardest cases:");
    for (query, recall) in worst.iter().take(5) {
        eprintln!("  {recall:.2}  {query}");
    }
    assert!(
        mean_recall >= 0.8,
        "mean recall@8 {mean_recall:.4} < 0.8; hardest: {:?}",
        &worst[..worst.len().min(5)]
    );

    // ---- Top-k beats full injection under the budget ------------------------
    let full_injection_tokens: usize = items.iter().map(full_definition_tokens).sum();
    eprintln!(
        "disclosed peak = {disclosed_token_peak} tokens; budget = {BUDGET_TOKENS}; \
         full injection = {full_injection_tokens} tokens"
    );
    assert!(
        disclosed_token_peak <= BUDGET_TOKENS,
        "disclosed {disclosed_token_peak} tokens exceeds budget {BUDGET_TOKENS}"
    );
    assert!(
        BUDGET_TOKENS < full_injection_tokens,
        "full injection {full_injection_tokens} did not exceed budget {BUDGET_TOKENS}"
    );
}

/// The risk ceiling — not mere absence — is what excludes the destructive
/// decoys. A query that names one directly ranks it top on exact overlap; it is
/// disclosed under a High ceiling, yet vanishes the instant the ceiling drops to
/// Medium. This proves the forbidden-exclusion result above is the hard filter's
/// doing, and never a ranking accident.
#[tokio::test]
async fn risk_ceiling_is_the_hard_filter_that_excludes_destructive_items() {
    let (_tmp, items, repo) = seed().await;
    let indexes = RetrievalIndexes::build(&items, HashingEmbedder::new()).unwrap();
    let config = RetrievalConfig {
        disclose_tools_max: 8,
        ..RetrievalConfig::default()
    };

    let text = "drop the production database table permanently";
    let visible = vec![Scope::System, Scope::Repository(repo)];

    let disclosed_under = |ceiling: RiskClass| -> bool {
        let query = RetrievalQuery::new(text, visible.clone(), ceiling);
        let result = retrieve(&items, &indexes, &query, &config).unwrap();
        result
            .tools
            .iter()
            .any(|card| card.name == "decoy.database_drop_table")
    };

    assert!(
        disclosed_under(RiskClass::High),
        "a High ceiling should admit the (top-ranked) destructive decoy"
    );
    assert!(
        !disclosed_under(RiskClass::Medium),
        "a Medium ceiling must filter the High-risk destructive decoy"
    );
}

/// Parse the JSONL fixtures.
fn load_cases() -> Vec<EvalCase> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/retrieval-eval.jsonl");
    let raw = std::fs::read_to_string(&path).expect("read eval fixtures");
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse eval case"))
        .collect()
}

/// A ~4-chars/token estimate.
fn est_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// The token cost of injecting an item's **full** definition (name, description,
/// intents, keywords, both JSON schemas, and permissions) — the "full tool
/// injection" baseline the funnel improves on.
fn full_definition_tokens(item: &RegistryItem) -> usize {
    let mut text = format!(
        "{} {} {} {}",
        item.name,
        item.description,
        item.intents.join(" "),
        item.keywords.join(" ")
    );
    if let Some(schema) = &item.input_schema {
        text.push_str(&schema.to_string());
    }
    if let Some(schema) = &item.output_schema {
        text.push_str(&schema.to_string());
    }
    text.push_str(&serde_json::to_string(&item.permissions).unwrap());
    est_tokens(&text)
}

/// The 20 synthetic decoys: three High-risk destructive tools that must be
/// filtered, plus seventeen plausible, unrelated Medium/Low/Safe tools that must
/// be out-ranked. Every decoy carries a bulky input schema so the full-injection
/// baseline is realistically large.
fn decoys() -> Vec<RegistryItem> {
    vec![
        // --- Three High-risk destructive decoys (Command + Secret ⇒ High) ----
        decoy(
            "decoy.database_drop_table",
            "Permanently drop a table and all of its rows from the production Postgres database.",
            &["delete a table", "remove database rows"],
            &["database", "postgres", "drop", "table", "schema"],
            vec![
                CapabilityRequest::Command("psql".into()),
                CapabilityRequest::FilesystemWrite("/var/lib/postgres".into()),
                CapabilityRequest::Secret("db-password".into()),
            ],
        ),
        decoy(
            "decoy.aws_s3_delete",
            "Recursively delete every object under an S3 bucket prefix, with no recovery.",
            &["empty an s3 bucket", "remove cloud storage"],
            &["aws", "s3", "bucket", "destroy", "object"],
            vec![
                CapabilityRequest::Command("aws".into()),
                CapabilityRequest::Network("s3.amazonaws.com:443".into()),
                CapabilityRequest::Secret("aws-credentials".into()),
            ],
        ),
        decoy(
            "decoy.k8s_delete_namespace",
            "Delete a Kubernetes namespace and every workload, secret, and volume inside it.",
            &["tear down a namespace", "remove cluster resources"],
            &["kubernetes", "namespace", "destroy", "cluster", "workload"],
            vec![
                CapabilityRequest::Command("kubectl".into()),
                CapabilityRequest::Network("cluster.local:6443".into()),
                CapabilityRequest::Secret("kubeconfig".into()),
            ],
        ),
        // --- Seventeen unrelated Medium/Low/Safe distractors -----------------
        decoy(
            "decoy.docker_build",
            "Assemble a container image from a Dockerfile and push it to a registry.",
            &["containerize an application", "produce an image"],
            &["docker", "container", "image", "dockerfile", "registry"],
            vec![CapabilityRequest::Command("docker".into())],
        ),
        decoy(
            "decoy.npm_install",
            "Install the JavaScript dependencies declared in package.json from the npm registry.",
            &["install node dependencies", "add a js package"],
            &["npm", "javascript", "node", "dependencies", "package"],
            vec![CapabilityRequest::Command("npm".into())],
        ),
        decoy(
            "decoy.http_request",
            "Issue an HTTP request to a REST endpoint and return the decoded response body.",
            &["call a rest api", "fetch a url"],
            &["http", "rest", "request", "endpoint", "url"],
            vec![CapabilityRequest::Network("*".into())],
        ),
        decoy(
            "decoy.slack_post",
            "Post a message to a Slack channel through the Slack web API.",
            &["send a slack message", "notify a channel"],
            &["slack", "message", "channel", "notify", "chat"],
            vec![CapabilityRequest::Network("slack.com:443".into())],
        ),
        decoy(
            "decoy.pdf_extract",
            "Extract the plain text and tables from a PDF document.",
            &["parse a pdf", "pull text from a document"],
            &["pdf", "document", "extract", "text", "table"],
            vec![CapabilityRequest::FilesystemRead("$REPOSITORY".into())],
        ),
        decoy(
            "decoy.csv_parse",
            "Parse CSV content into typed rows and columns.",
            &["load a csv", "tabulate data"],
            &["csv", "rows", "columns", "tabular", "spreadsheet"],
            vec![CapabilityRequest::FilesystemRead("$REPOSITORY".into())],
        ),
        decoy(
            "decoy.image_resize",
            "Resize and re-encode an image to target dimensions.",
            &["shrink an image", "make a thumbnail"],
            &["image", "resize", "scale", "thumbnail", "dimensions"],
            vec![CapabilityRequest::FilesystemRead("$REPOSITORY".into())],
        ),
        decoy(
            "decoy.regex_match",
            "Match text against a regular expression and return the capture groups.",
            &["test a regular expression", "extract capture groups"],
            &["regex", "regexp", "capture", "expression", "groups"],
            Vec::new(),
        ),
        decoy(
            "decoy.json_format",
            "Pretty-print and validate a JSON payload.",
            &["format json", "prettify a payload"],
            &["json", "format", "pretty", "validate", "payload"],
            Vec::new(),
        ),
        decoy(
            "decoy.terraform_plan",
            "Compute an execution plan for a Terraform configuration.",
            &["preview infrastructure updates", "plan terraform"],
            &["terraform", "infrastructure", "plan", "hcl", "provisioning"],
            vec![
                CapabilityRequest::Command("terraform".into()),
                CapabilityRequest::Network("*".into()),
            ],
        ),
        decoy(
            "decoy.jira_create",
            "Create a Jira issue in a project backlog.",
            &["log a jira ticket", "raise an issue"],
            &["jira", "issue", "ticket", "backlog", "tracker"],
            vec![CapabilityRequest::Network("jira.example.com:443".into())],
        ),
        decoy(
            "decoy.calendar_schedule",
            "Schedule a calendar event and invite attendees.",
            &["book a meeting", "add a calendar event"],
            &["calendar", "event", "schedule", "meeting", "invite"],
            vec![CapabilityRequest::Network("*".into())],
        ),
        decoy(
            "decoy.email_send",
            "Send an email message to one or more recipients.",
            &["send an email", "email a report"],
            &["email", "mail", "send", "smtp", "recipient"],
            vec![CapabilityRequest::Network("smtp.example.com:465".into())],
        ),
        decoy(
            "decoy.browser_screenshot",
            "Capture a full-page screenshot of a web page in a headless browser.",
            &["screenshot a page", "capture a website"],
            &["browser", "screenshot", "headless", "render", "webpage"],
            vec![
                CapabilityRequest::Command("chromium".into()),
                CapabilityRequest::Network("*".into()),
            ],
        ),
        decoy(
            "decoy.aws_s3_upload",
            "Upload a local object to an S3 bucket.",
            &["put an object on s3", "upload to cloud storage"],
            &["aws", "s3", "bucket", "upload", "object"],
            vec![CapabilityRequest::Network("s3.amazonaws.com:443".into())],
        ),
        decoy(
            "decoy.k8s_apply",
            "Apply a Kubernetes manifest to a cluster.",
            &["deploy to kubernetes", "roll out a manifest"],
            &["kubernetes", "manifest", "deploy", "cluster", "rollout"],
            vec![
                CapabilityRequest::Command("kubectl".into()),
                CapabilityRequest::Network("cluster.local:6443".into()),
            ],
        ),
        decoy(
            "decoy.python_run",
            "Invoke a Python script through the interpreter.",
            &["invoke a python script", "evaluate python code"],
            &["python", "script", "interpreter", "pip", "venv"],
            vec![CapabilityRequest::Command("python".into())],
        ),
    ]
}

/// Build one decoy tool. Risk is derived from permissions exactly as real items
/// derive theirs, so the destructive three come out High (a `Secret` capability
/// pushes them past Medium) and the rest land Low/Medium/Safe.
fn decoy(
    name: &str,
    description: &str,
    intents: &[&str],
    keywords: &[&str],
    permissions: Vec<CapabilityRequest>,
) -> RegistryItem {
    let now = chrono::Utc::now();
    let risk = RiskClass::from_permissions(&permissions);
    RegistryItem {
        id: RegistryItemId::new(),
        kind: RegistryItemKind::Tool,
        name: name.to_string(),
        version: Version("1.0.0".to_string()),
        // System-scoped so scope never filters them — the risk ceiling is what
        // must exclude the destructive ones.
        scope: Scope::System,
        description: description.to_string(),
        intents: intents.iter().map(|s| s.to_string()).collect(),
        keywords: keywords.iter().map(|s| s.to_string()).collect(),
        examples: vec![UsageExample {
            query: format!("use {name}"),
            note: None,
        }],
        // A bulky schema so the full-injection baseline is realistically large.
        input_schema: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "description": "The primary resource this operation acts upon." },
                "options": { "type": "object", "description": "Provider-specific options controlling the operation." },
                "dry_run": { "type": "boolean", "description": "Preview the effect without performing it." },
                "timeout_seconds": { "type": "integer", "description": "Abort if the operation exceeds this many seconds." }
            },
            "required": ["target"]
        })),
        output_schema: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "description": "Terminal status of the operation." },
                "detail": { "type": "string", "description": "A human-readable result summary." }
            }
        })),
        dependencies: Vec::new(),
        permissions,
        risk,
        provenance: Provenance::BuiltIn,
        trust: TrustMetadata {
            publisher: "community-registry".to_string(),
            signature_required: false,
            signature: None,
            tier: TrustTier::Community,
        },
        status: RegistryStatus::Active,
        content_hash: String::new(),
        executable: true,
        created_at: now,
        updated_at: now,
    }
}
