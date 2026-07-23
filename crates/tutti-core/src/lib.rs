mod domain;
mod frame;
mod ids;
mod paths;
mod protocol;
mod state;

pub use domain::{AgentKind, Direction, Layout};
pub use frame::{Frame, FrameError, MAX_FRAME_LEN, PaneData};
pub use ids::{PaneId, TabId, WorkspaceId};
pub use paths::{socket_dir, socket_path};
pub use protocol::{
    AgentHookEvent, Event, PaneInfo, Request, Response, SubagentInfo, TabInfo, TabView, WIRE_REV,
    WorkspaceInfo, WorkspaceView,
};
pub use state::{AgentState, Observation, StateEvent};
