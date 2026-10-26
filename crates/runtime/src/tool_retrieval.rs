use std::time::Duration;

use codypendent_knowledge::{
    embedding_text, EmbedError, HashingEmbedder, RegistryItem, RetrievalError, RetrievalIndexes,
    SemanticEmbedder, VectorIndex,
};

const SELECTION_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn tool_indexes(
    items: &[RegistryItem],
    query: &str,
    semantic: Option<&dyn SemanticEmbedder>,
) -> Result<(RetrievalIndexes, Option<Vec<f32>>), RetrievalError> {
    if let Some(semantic) = semantic {
        match tokio::time::timeout(SELECTION_TIMEOUT, semantic_vectors(items, query, semantic))
            .await
        {
            Ok(Ok((index, query))) => {
                return Ok((
                    RetrievalIndexes::from_parts(items, index, HashingEmbedder::new())?,
                    Some(query),
                ));
            }
            Ok(Err(error)) => {
                tracing::warn!(model = semantic.model(), %error, "semantic tool selection unavailable; using lexical retrieval")
            }
            Err(_) => tracing::warn!(
                model = semantic.model(),
                "semantic tool selection exceeded its two-second budget; using lexical retrieval"
            ),
        }
    }
    Ok((
        RetrievalIndexes::build(items, HashingEmbedder::new())?,
        None,
    ))
}

async fn semantic_vectors(
    items: &[RegistryItem],
    query: &str,
    semantic: &dyn SemanticEmbedder,
) -> Result<(VectorIndex, Vec<f32>), EmbedError> {
    let texts: Vec<_> = items.iter().map(embedding_text).collect();
    let (vectors, query) =
        tokio::try_join!(semantic.embed_batch(&texts), semantic.embed_query(query),)?;
    if vectors.len() != items.len()
        || query.is_empty()
        || std::iter::once(&query).chain(&vectors).any(|vector| {
            vector.len() != query.len()
                || vector.iter().any(|value| !value.is_finite())
                || !vector.iter().any(|value| *value != 0.0)
        })
    {
        return Err(EmbedError("invalid tool embedding vectors".to_string()));
    }
    let mut index = VectorIndex::new();
    for (item, vector) in items.iter().zip(vectors) {
        index.insert(item.id, vector);
    }
    Ok((index, query))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_knowledge::builtin_tools;

    struct ParaphraseEmbedder;

    #[async_trait::async_trait]
    impl SemanticEmbedder for ParaphraseEmbedder {
        fn model(&self) -> &str {
            "paraphrase"
        }

        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|text| {
                    if text.starts_with("workspace.read_file ") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }

        async fn embed_query(&self, query: &str) -> Result<Vec<f32>, EmbedError> {
            assert_eq!(query, "inspect the implementation");
            Ok(vec![1.0, 0.0])
        }
    }

    #[tokio::test]
    async fn semantic_tool_selection_uses_document_and_query_embeddings() {
        let items = builtin_tools();
        let expected = items
            .iter()
            .find(|item| item.name == "workspace.read_file")
            .expect("read tool")
            .id;
        let (indexes, query) = tool_indexes(
            &items,
            "inspect the implementation",
            Some(&ParaphraseEmbedder),
        )
        .await
        .unwrap();
        assert_eq!(query, Some(vec![1.0, 0.0]));
        assert_eq!(indexes.vector.search(&query.unwrap(), 1)[0].0, expected);
    }

    struct BrokenEmbedder;

    #[async_trait::async_trait]
    impl SemanticEmbedder for BrokenEmbedder {
        fn model(&self) -> &str {
            "broken"
        }

        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(vec![vec![f32::NAN]; texts.len()])
        }

        async fn embed_query(&self, _: &str) -> Result<Vec<f32>, EmbedError> {
            Ok(vec![1.0, 0.0])
        }
    }

    #[tokio::test]
    async fn invalid_semantic_vectors_fall_back_as_a_complete_pair() {
        let items = builtin_tools();
        let (indexes, query) = tool_indexes(&items, "read source", Some(&BrokenEmbedder))
            .await
            .unwrap();
        assert!(query.is_none());
        assert_eq!(
            indexes
                .vector
                .search(&vec![0.1; codypendent_knowledge::EMBEDDING_DIMENSION], 1)
                .len(),
            1,
        );
    }

    struct StalledEmbedder;

    #[async_trait::async_trait]
    impl SemanticEmbedder for StalledEmbedder {
        fn model(&self) -> &str {
            "stalled"
        }

        async fn embed_batch(&self, _: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_provider_does_not_hold_up_tool_selection() {
        let started = tokio::time::Instant::now();
        let (_, query) = tool_indexes(&builtin_tools(), "read source", Some(&StalledEmbedder))
            .await
            .unwrap();
        assert!(query.is_none());
        assert_eq!(started.elapsed(), SELECTION_TIMEOUT);
    }
}
