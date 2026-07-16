//! The BM25 lexical index (Chapter 05, STEP 2.3), built on Tantivy.
//!
//! Lexical retrieval catches what dense retrieval blurs: exact identifiers,
//! command names, and keywords the user typed verbatim. The index carries the
//! four text fields the doc names — `name`, `description`, `intents`, `keywords`
//! — plus the stored [`RegistryItemId`] so a hit maps back to its authoritative
//! row. Phase 2 uses an in-RAM index ([`Index::create_in_ram`]); like the vector
//! index it is derived and rebuildable from [`Registry::list`](crate::registry::Registry::list).

use std::str::FromStr;

use codypendent_protocol::RegistryItemId;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, STORED, STRING, TEXT};
use tantivy::{Index, IndexReader, TantivyDocument};

use crate::types::RegistryItem;

/// A failure building or querying the lexical index.
#[derive(Debug, thiserror::Error)]
pub enum Bm25Error {
    /// A Tantivy operation failed (schema, writer, reader, or search).
    #[error(transparent)]
    Tantivy(#[from] tantivy::TantivyError),
}

/// The schema field handles, kept so search can build a [`QueryParser`] over the
/// same fields the documents were written with.
#[derive(Debug, Clone, Copy)]
struct Fields {
    id: Field,
    name: Field,
    description: Field,
    intents: Field,
    keywords: Field,
}

/// A BM25 index over the registry's text fields.
pub struct Bm25Index {
    index: Index,
    reader: IndexReader,
    fields: Fields,
}

impl std::fmt::Debug for Bm25Index {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bm25Index").finish_non_exhaustive()
    }
}

impl Bm25Index {
    /// Build an in-RAM index over `items`.
    ///
    /// Each item contributes its `name`, `description`, `intents` (joined), and
    /// `keywords` (joined) as searchable text, with its id stored for retrieval.
    pub fn build(items: &[RegistryItem]) -> Result<Self, Bm25Error> {
        let mut schema_builder = Schema::builder();
        // The id is stored verbatim (untokenized) purely to map a hit back to its
        // row; the four content fields are tokenized for BM25.
        let fields = Fields {
            id: schema_builder.add_text_field("id", STRING | STORED),
            name: schema_builder.add_text_field("name", TEXT),
            description: schema_builder.add_text_field("description", TEXT),
            intents: schema_builder.add_text_field("intents", TEXT),
            keywords: schema_builder.add_text_field("keywords", TEXT),
        };
        let schema = schema_builder.build();
        let index = Index::create_in_ram(schema);

        {
            let mut writer = index.writer(15_000_000)?;
            for item in items {
                let mut doc = TantivyDocument::new();
                doc.add_text(fields.id, item.id.to_string());
                doc.add_text(fields.name, &item.name);
                doc.add_text(fields.description, &item.description);
                doc.add_text(fields.intents, item.intents.join(" "));
                doc.add_text(fields.keywords, item.keywords.join(" "));
                writer.add_document(doc)?;
            }
            writer.commit()?;
        }

        let reader = index.reader()?;
        Ok(Self {
            index,
            reader,
            fields,
        })
    }

    /// The `top_k` items whose text best matches `query` by BM25, highest first.
    ///
    /// The query is reduced to lowercase alphanumeric terms before parsing, so
    /// punctuation in a natural-language task (paths, `cargo test`, `src/main.rs`)
    /// never trips the query grammar; terms are combined disjunctively for recall.
    pub fn search(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<(RegistryItemId, f32)>, Bm25Error> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let sanitized = sanitize(query);
        if sanitized.trim().is_empty() {
            return Ok(Vec::new());
        }

        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.name,
                self.fields.description,
                self.fields.intents,
                self.fields.keywords,
            ],
        );
        // Lenient parsing never errors on a stray term; any per-term diagnostics
        // are irrelevant once punctuation is already stripped.
        let (parsed, _errors) = parser.parse_query_lenient(&sanitized);
        let hits = searcher.search(&parsed, &TopDocs::with_limit(top_k))?;

        let mut out = Vec::with_capacity(hits.len());
        for (score, address) in hits {
            let doc: TantivyDocument = searcher.doc(address)?;
            if let Some(id) = doc
                .get_first(self.fields.id)
                .and_then(|value| value.as_str())
                .and_then(|raw| RegistryItemId::from_str(raw).ok())
            {
                out.push((id, score));
            }
        }
        Ok(out)
    }
}

/// Replace every non-alphanumeric character with a space and lowercase the rest,
/// so the query parser sees only bare, tokenizable terms.
fn sanitize(query: &str) -> String {
    query
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect()
}
