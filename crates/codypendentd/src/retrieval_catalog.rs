use chrono::Utc;
use codypendent_integrations::mcp::McpBridge;
use codypendent_knowledge::{
    Provenance, RegistryItem, RegistryItemKind, RegistryStatus, RiskClass, Scope, TrustMetadata,
    TrustTier, Version,
};
use codypendent_protocol::RegistryItemId;
use sha2::{Digest, Sha256};

pub(crate) fn offered_items(bridge: Option<&dyn McpBridge>) -> Vec<RegistryItem> {
    let Some(bridge) = bridge else {
        return Vec::new();
    };
    bridge
        .offered_tools()
        .into_iter()
        .map(|tool| {
            let name = format!("mcp.{}.{}", tool.server, tool.name);
            let digest = Sha256::digest(format!("codypendent:tool:{name}"));
            let mut id = [0; 16];
            id.copy_from_slice(&digest[..16]);
            id[6] = (id[6] & 0x0f) | 0x80;
            id[8] = (id[8] & 0x3f) | 0x80;
            RegistryItem {
                id: RegistryItemId(uuid::Uuid::from_bytes(id)),
                name,
                kind: RegistryItemKind::Tool,
                version: Version("1.0.0".to_string()),
                scope: Scope::System,
                description: tool.description,
                intents: Vec::new(),
                keywords: vec![tool.server.clone(), tool.name],
                examples: Vec::new(),
                input_schema: Some(tool.input_schema),
                output_schema: None,
                dependencies: Vec::new(),
                permissions: Vec::new(),
                risk: RiskClass::Medium,
                provenance: Provenance::Package {
                    path: format!("mcp://{}", tool.server),
                },
                trust: TrustMetadata {
                    publisher: tool.server,
                    signature_required: false,
                    signature: None,
                    tier: TrustTier::Community,
                },
                status: RegistryStatus::Active,
                content_hash: String::new(),
                executable: true,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }
        })
        .collect()
}

pub(crate) fn policy_marker(item: &RegistryItem) -> Option<String> {
    match &item.provenance {
        Provenance::Package { path } => path
            .strip_prefix("mcp://")
            .map(|server| format!("mcp-policy:{server}")),
        Provenance::BuiltIn => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use codypendent_integrations::mcp::{McpError, McpToolInfo};
    use codypendent_runtime::tools::{RegistrySearch, RegistrySearchRequest};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    struct LiveCatalog(Mutex<Vec<McpToolInfo>>);

    #[async_trait]
    impl McpBridge for LiveCatalog {
        fn offered_tools(&self) -> Vec<McpToolInfo> {
            self.0.lock().unwrap().clone()
        }

        async fn call_tool(&self, _: &str, _: &str, _: Value) -> Result<String, McpError> {
            panic!("discovery must not execute integration tools")
        }
    }

    #[tokio::test]
    async fn live_mcp_search_opens_the_real_schema_without_persisting_transient_items() {
        let directory = tempfile::tempdir().unwrap();
        let pool = codypendent_knowledge::db::open(&directory.path().join("catalog.db"))
            .await
            .unwrap();
        let schema =
            json!({"type":"object","properties":{"date":{"type":"string"}},"required":["date"]});
        let bridge = Arc::new(LiveCatalog(Mutex::new(vec![McpToolInfo {
            server: "calendar".to_string(),
            name: "meetings".to_string(),
            description: "Find meetings and calendar availability".to_string(),
            input_schema: schema.clone(),
        }])));
        let search = crate::retrieval::PoolRegistrySearch::new(pool.clone(), None)
            .with_mcp_catalog(Some(bridge.clone()));
        let request = RegistrySearchRequest {
            query: "find calendar meetings",
            open: Some("mcp.calendar.meetings"),
            repository: directory.path(),
        };
        let found = search.search(request).await.unwrap();
        assert_eq!(found.tool_schema.unwrap().parameters, schema);
        assert!(found.cards[0]
            .permissions
            .contains(&"mcp-policy:calendar".to_string()));
        assert!(codypendent_knowledge::Registry::new()
            .list(&pool)
            .await
            .unwrap()
            .is_empty());
        let vectors: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM registry_embeddings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(vectors, 0);

        bridge.0.lock().unwrap().clear();
        let gone = search.search(request).await.unwrap();
        assert!(gone.cards.is_empty());
        assert!(gone.tool_schema.is_none());
        assert!(gone.open_note.is_some());

        let without_catalog = crate::retrieval::PoolRegistrySearch::new(pool, None)
            .search(request)
            .await
            .unwrap();
        assert!(without_catalog.cards.is_empty());
    }
}
