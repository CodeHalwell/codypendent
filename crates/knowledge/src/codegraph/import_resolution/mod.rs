//! Conservative cross-file calls backed by explicit source imports.

use std::collections::{HashMap, HashSet};

use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};

use super::*;

#[derive(Clone)]
struct Binding {
    local: String,
    module: String,
    symbol: Option<String>,
}

#[derive(Default)]
pub(in crate::codegraph) struct Resolution {
    pub calls: HashMap<(usize, usize), CodeNodeId>,
    pub imports: HashMap<String, CodeNodeId>,
    pub relations: Vec<ResolvedRelation>,
}

pub(in crate::codegraph) struct ResolvedRelation {
    pub from: String,
    pub to: CodeNodeId,
    pub relation: CodeRelation,
    pub site: (usize, usize),
}

mod bindings;
mod dependents;
mod paths;
mod persistence;
mod resolver;
mod syntax;

use bindings::*;
pub(super) use dependents::affected_importers_page;
use paths::*;
pub(super) use persistence::{add_relations, prune_references, reconcile_file};
pub(super) use resolver::resolve;
use syntax::*;
