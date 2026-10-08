//! Craft Apps Host updater: installs official Craft web releases for static serving and keeps
//! them up to date without rebuilding images or restarting the web server.

pub mod archive;
pub mod config;
pub mod daemon;
pub mod doctor;
pub mod fsutil;
pub mod github;
pub mod glob;
pub mod layout;
pub mod logging;
pub mod ops;
pub mod status;
pub mod store;
pub mod timeutil;
pub mod validate;
pub mod version;

/// Archive builders shared by unit and integration tests; not part of the operator interface.
#[doc(hidden)]
pub mod testutil;
