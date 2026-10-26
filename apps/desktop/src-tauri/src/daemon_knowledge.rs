use super::*;
use codypendent_protocol::knowledge::{KnowledgePage, KnowledgeSection};

impl DaemonClient {
    async fn selected_knowledge_session(&self) -> anyhow::Result<SessionId> {
        if let Some(session) = *self.attached.lock().await {
            return Ok(session);
        }
        let mut held = self.knowledge_session.lock().await;
        if let Some(session) = *held {
            return Ok(session);
        }
        let reply = self
            .send_command(CommandBody::CreateSession {
                workspace: self.workspace,
                title: "Workspace knowledge".into(),
                repository: self.repository.clone(),
                internal: true,
                parent_session_id: None,
                parent_run_id: None,
            })
            .await?;
        let session = match reply.payload {
            Payload::CommandAccepted { .. } => reply
                .session_id
                .context("The daemon did not identify the knowledge session")?,
            Payload::CommandRejected(error) => bail!("{} ({})", error.message, error.code),
            _ => bail!("The daemon did not create a knowledge session"),
        };
        *held = Some(session);
        Ok(session)
    }

    async fn knowledge_page(
        &self,
        session_id: SessionId,
        section: KnowledgeSection,
        offset: u32,
        limit: u32,
    ) -> anyhow::Result<KnowledgePage> {
        let reply = self
            .send_command(CommandBody::ReadKnowledge {
                session_id,
                section,
                offset,
                limit,
            })
            .await?;
        let page = match reply.payload {
            Payload::KnowledgePage { page, .. } if page.section == section => *page,
            Payload::CommandRejected(error) => bail!("{} ({})", error.message, error.code),
            _ => bail!("Unexpected knowledge response; update the daemon and reconnect"),
        };
        if self.selected_knowledge_session().await? != session_id {
            bail!("The selected session changed; refresh knowledge");
        }
        Ok(page)
    }

    pub async fn read_knowledge(&self, section: KnowledgeSection) -> anyhow::Result<KnowledgePage> {
        let session = self.selected_knowledge_session().await?;
        let mut result = self.knowledge_page(session, section, 0, 100).await?;
        let mut offset = 0;
        while let Some(next) = result.next_offset {
            if next <= offset || next >= 10_000 {
                bail!("Knowledge browsing reached its 10,000-item limit or made no progress; narrow the repository scope");
            }
            let page = self.knowledge_page(session, section, next, 100).await?;
            if page.repository != result.repository || page.workspace_id != result.workspace_id {
                bail!("The session's knowledge scope changed; refresh knowledge");
            }
            result.skills.extend(page.skills);
            result.memories.extend(page.memories);
            result.learnings.extend(page.learnings);
            result.documents.extend(page.documents);
            result.warnings.extend(page.warnings);
            result.next_offset = page.next_offset;
            offset = next;
        }
        Ok(result)
    }

    pub(super) async fn knowledge_repository(&self, what: &str) -> anyhow::Result<String> {
        let session = self.selected_knowledge_session().await?;
        self.knowledge_page(session, KnowledgeSection::Skills, 0, 1)
            .await?
            .repository
            .with_context(|| {
                format!("select a repository first: {what} have no repository in this session")
            })
    }

    pub(super) async fn ensure_selected_document(
        &self,
        document: DocumentId,
    ) -> anyhow::Result<()> {
        if !self
            .read_knowledge(KnowledgeSection::Documents)
            .await?
            .documents
            .iter()
            .any(|item| item.document_id == document)
        {
            bail!("The document is not visible in the selected session; refresh documents");
        }
        Ok(())
    }

    pub async fn mutate_learning(
        &self,
        id: &str,
        revision: u64,
        mutation: &crate::knowledge::LearningMutation,
    ) -> anyhow::Result<String> {
        let _selection = self.attach_lock.lock().await;
        let page = self.read_knowledge(KnowledgeSection::Learnings).await?;
        let record = page
            .learnings
            .iter()
            .find(|record| record.id == id)
            .context("The learning is not visible in the selected session; refresh learnings")?;
        if record.revision != revision {
            bail!("The learning changed; refresh learnings before editing it");
        }
        crate::knowledge::mutate_learning(id, revision, mutation).await
    }
}
