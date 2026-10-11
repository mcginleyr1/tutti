//! The `tutti` client: a control-plane `Client` for the CLI verbs, table
//! rendering for their output, the agent integrations it installs, the issue
//! trackers it deploys from, the editor panes it hands files to, the
//! daemon's machine it asks about (`host`, served across ssh by `tutti
//! proxy`), and the attachable `attach` TUI.

pub mod attach;
pub mod client;
pub mod config;
pub mod editor;
pub mod hooks;
pub mod host;
pub mod integrations;
pub mod issues;
pub mod render;
pub mod sandbox;
