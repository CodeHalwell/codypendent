use super::KnowledgeIdentity;
use anyhow::{bail, ensure, Context};
use codypendent_protocol::{CommandBody, SessionId};
use sqlx::SqlitePool;
use std::path::PathBuf;

pub async fn session_identity(
    pool: &SqlitePool,
    session: SessionId,
) -> anyhow::Result<KnowledgeIdentity> {
    let (workspace,): (Option<String>,) =
        sqlx::query_as("SELECT workspace_id FROM sessions WHERE id = ? AND tombstoned_at IS NULL")
            .bind(session.to_string())
            .fetch_optional(pool)
            .await?
            .context("session not found")?;
    Ok(KnowledgeIdentity {
        workspace: workspace
            .map(|id| id.parse())
            .transpose()
            .context("invalid session workspace identity")?,
        repository: session_repository(pool, session).await?,
    })
}

async fn session_repository(
    pool: &SqlitePool,
    mut session: SessionId,
) -> anyhow::Result<Option<PathBuf>> {
    for _ in 0..=8 {
        let (repository, parent): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT repository, forked_from_session_id FROM sessions WHERE id = ?")
                .bind(session.to_string())
                .fetch_optional(pool)
                .await?
                .context("session provenance not found")?;
        if let Some(repository) = repository {
            return Ok(Some(PathBuf::from(repository)));
        }
        let bodies: Vec<(String,)> = sqlx::query_as(
            "SELECT body FROM commands WHERE session_id = ? AND status = 'applied' \
             AND body LIKE '%\"type\":\"StartRun\"%' ORDER BY rowid DESC",
        )
        .bind(session.to_string())
        .fetch_all(pool)
        .await?;
        for (body,) in bodies {
            if let CommandBody::StartRun {
                session_id,
                repository,
                ..
            } = serde_json::from_str::<CommandBody>(&body)
                .context("invalid applied session repository command")?
            {
                ensure!(
                    session_id == session,
                    "session repository command identity mismatch"
                );
                return Ok(repository.map(PathBuf::from));
            }
        }
        let Some(parent) = parent else {
            return Ok(None);
        };
        session = parent.parse().context("invalid parent session identity")?;
    }
    bail!("session repository provenance exceeds the fork depth limit")
}
