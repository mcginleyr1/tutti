mod domain;
mod frame;
mod ids;
mod paths;
mod protocol;
mod state;

pub use domain::{AgentKind, Direction, Layout, Pane, Tab, Workspace};
pub use frame::{Frame, FrameError, MAX_FRAME_LEN, PaneData};
pub use ids::{PaneId, TabId, WorkspaceId};
pub use paths::{socket_dir, socket_path};
pub use protocol::{
    Event, PaneInfo, Request, Response, TabInfo, TabView, WorkspaceInfo, WorkspaceView,
};
pub use state::{AgentState, Observation, StateEvent};
