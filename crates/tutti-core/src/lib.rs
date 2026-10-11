mod domain;
mod frame;
mod ids;
mod paths;
mod prompt;
mod protocol;
mod sandbox;
mod state;

pub use domain::{AgentKind, Direction, Layout, Toward};
pub use frame::{Frame, FrameError, MAX_FRAME_LEN, PaneData};
pub use ids::{PaneId, TabId, WorkspaceId};
pub use paths::{log_path, socket_dir, socket_path, state_dir};
pub use prompt::{PROMPT_LINES, prompt_fingerprint, prompt_rows};
pub use protocol::{
    AgentHookEvent, ApprovalDecision, Checkpoint, DIFF_RANGE, Event, FileStat, IssueRef, NOTES_CAP,
    Note, NoteKind, PaneDims, PaneInfo, PendingApproval, Request, Response, RestoreSummary,
    SubagentInfo, TabInfo, TabView, Tagged, WIRE_REV, WaitFor, WatchPane, WorkspaceInfo,
    WorkspaceView,
};
pub use sandbox::{pick_remote, sandbox_branch};
pub use state::{AgentState, Observation, StateEvent, StateSource};
