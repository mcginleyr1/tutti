//! The `tutti` client: a control-plane `Client` for the CLI verbs, table
//! rendering for their output, the agent integrations it installs, and the
//! attachable `attach` TUI.

pub mod attach;
pub mod client;
pub mod config;
pub mod hooks;
pub mod integrations;
pub mod render;
pub mod sandbox;
