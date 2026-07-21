mod domain;
mod ids;
mod protocol;
mod state;

pub use domain::{AgentKind, Direction, Layout, Pane, Tab, Workspace};
pub use ids::{PaneId, TabId, WorkspaceId};
pub use protocol::{Event, PaneInfo, Request, Response, TabInfo, WorkspaceInfo};
pub use state::{AgentState, Observation, StateEvent};
