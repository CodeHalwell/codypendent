//! codypendent-providers — the provider/auth data model, credential-provider
//! trait, and curated built-in catalog. A daemon-free, network-free leaf crate.

// `catalog` lands in a later task of the universal-providers plan (Task 3);
// this crate currently ships the Task 1 data model + Task 2 credential seam.
// pub mod catalog;
pub mod credential;
pub mod model;

// pub use catalog::{builtin_providers, Catalog, CatalogError};
pub use credential::{credential_for, CredentialError, CredentialProvider, ResolvedCredential};
pub use model::{AuthMethod, Model, Protocol, Provider, ProvidersFile};
