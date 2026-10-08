//! Craft Apps Host: serves the official Craft browser apps under stable per-app paths, keeps them
//! updated from their GitHub releases, and offers an administration interface.

pub mod access;
pub mod activity;
pub mod admin;
pub mod archive;
pub mod assets;
pub mod auth;
pub mod candidate;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod fsutil;
pub mod github;
pub mod glob;
pub mod layout;
pub mod logging;
pub mod oidc;
pub mod ops;
pub mod precompress;
pub mod serve;
pub mod server;
pub mod status;
pub mod store;
pub mod timeutil;
pub mod validate;
pub mod version;

/// Archive builders shared by unit and integration tests; not part of the operator interface.
#[doc(hidden)]
pub mod testutil;
