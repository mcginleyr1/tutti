//! What a client needs to speak to tutti's daemon, and nothing of the daemon
//! or of any client: where the daemon listens, the frames on its socket, the
//! requests, responses and events they carry with the types those name, and
//! the revision of it all. `docs/transport-decision.md` is its spec.
//! `tutti-core` re-exports every item here, so tutti's own crates name them
//! there; a client outside tutti's workspace depends on this crate alone.

mod domain;
mod frame;
mod ids;
mod paths;
mod protocol;
mod state;

pub use domain::{AgentKind, Direction, Layout, Toward};
pub use frame::{Frame, FrameError, MAX_FRAME_LEN, PaneData};
pub use ids::{PaneId, TabId, WorkspaceId};
pub use paths::{socket_dir, socket_path};
pub use protocol::{
    AgentHookEvent, ApprovalDecision, Checkpoint, DIFF_RANGE, Event, FileStat, HostColors,
    IssueRef, NOTES_CAP, Note, NoteKind, PaneDims, PaneInfo, PendingApproval, Printed, Request,
    Response, RestoreSummary, Rgb, SubagentInfo, TabInfo, TabView, Tagged, WIRE_REV, WaitFor,
    WatchPane, WorkspaceInfo, WorkspaceView, row_hash,
};
pub use state::{AgentState, Observation, StateEvent, StateSource};
