//! codypendent-providers — the provider/auth data model, credential-provider
//! trait, and curated built-in catalog. A daemon-free, network-free leaf crate.

// `catalog` and `credential` land in later tasks of the universal-providers plan
// (Tasks 2-3); this crate currently ships only the Task 1 data model.
// pub mod catalog;
// pub mod credential;
pub mod model;

// pub use catalog::{builtin_providers, Catalog, CatalogError};
// pub use credential::{credential_for, CredentialError, CredentialProvider, ResolvedCredential};
pub use model::{AuthMethod, Model, Protocol, Provider, ProvidersFile};
