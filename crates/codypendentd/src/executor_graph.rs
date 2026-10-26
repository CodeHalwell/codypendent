use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use codypendent_daemon::poison::lock_recovering;
use codypendent_knowledge::codegraph::{CodeGraphQueries, GraphAnswer, GraphQuestion};
use codypendent_knowledge::GitRevision;
use codypendent_protocol::RepositoryId;
use sqlx::SqlitePool;

use crate::scan::{self, PoolCodeGraph};

pub(super) struct ReadyCodeGraph {
    graph: PoolCodeGraph,
    scanned: Arc<Mutex<HashMap<RepositoryId, GitRevision>>>,
}

impl ReadyCodeGraph {
    pub(super) fn new(
        pool: SqlitePool,
        scanned: Arc<Mutex<HashMap<RepositoryId, GitRevision>>>,
    ) -> Self {
        Self {
            graph: PoolCodeGraph::new(pool),
            scanned,
        }
    }
}

#[async_trait::async_trait]
impl CodeGraphQueries for ReadyCodeGraph {
    async fn ask(&self, root: &Path, question: GraphQuestion) -> Result<GraphAnswer, String> {
        let repository = scan::repository_id_for(root);
        let revision = scan::head_revision(root);
        if lock_recovering(&self.scanned).get(&repository) != Some(&revision) {
            return Err(
                "The code graph has not finished indexing this repository's current revision. \
                 Retry shortly or rebuild the code graph. Use workspace.search and \
                 workspace.read_file in the meantime."
                    .to_string(),
            );
        }
        self.graph.ask(root, question).await
    }
}
