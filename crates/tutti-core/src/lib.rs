mod domain;
mod frame;
mod ids;
mod paths;
mod prompt;
mod protocol;
mod state;

pub use domain::{AgentKind, Direction, Layout};
pub use frame::{Frame, FrameError, MAX_FRAME_LEN, PaneData};
pub use ids::{PaneId, TabId, WorkspaceId};
pub use paths::{socket_dir, socket_path, state_dir};
pub use prompt::{PROMPT_LINES, prompt_fingerprint, prompt_rows};
pub use protocol::{
    AgentHookEvent, Checkpoint, DIFF_RANGE, Event, FileStat, PaneInfo, Request, Response,
    RestoreSummary, SubagentInfo, TabInfo, TabView, WIRE_REV, WaitFor, WorkspaceInfo,
    WorkspaceView,
};
pub use state::{AgentState, Observation, StateEvent, StateSource};
