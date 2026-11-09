//! Follow-ups: what the reply to a tagged request completes — a jump to the
//! workspace it made, a merge's cleanup offer, a deploy's next step — and
//! what is given up on when the daemon refuses the request or never answers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tutti_core::{Frame as WireFrame, PaneId, Request, Response, WorkspaceId};

use super::app::{App, tagged};

/// What the reply to a tagged request completes (`App::follow`).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum FollowUp {
    /// A `WorkspaceNew` from add-project, whose refusal disarms the jump to
    /// the new workspace's tab.
    Mount,
    /// A `WorkspaceFork`: `WorkspaceCreated` jumps to the new workspace and
    /// opens its launcher.
    Fork,
    /// A `WorkspaceMerge` of this workspace: `Merged` offers its cleanup.
    Merge(WorkspaceId),
    /// A `WorkspaceFork` made for an issue: `WorkspaceCreated` sends this,
    /// the `Run` of the agent deployed on it, into the new workspace. With
    /// the prompt file the run names (`issues::deploy_argv`), which a
    /// refusal removes: no pane got it.
    Deploy(Box<Request>, Option<PathBuf>),
    /// A deploy's `Run`, with its prompt file as above: `Ran` jumps to the
    /// agent, and the daemon owns the file from then on.
    Deployed(Option<PathBuf>),
    /// A `PaneRead` of this pane's last reply: `LastReply` opens the reply
    /// overlay over the sidebar it was asked from.
    Reply(PaneId),
}

/// The tagged requests in flight.
#[derive(Default)]
pub(super) struct FollowUps {
    /// What the reply to each tagged request in flight completes, keyed by the
    /// `req` it rode out with: a guided create's jump and launcher, a merge's
    /// cleanup offer. Only that request's own reply — a refusal included —
    /// completes or cancels one, unless its deadline passes first
    /// (`expire_follow_ups`). Each goes with when it went out, and how long
    /// the CLI would wait on its request (`client::deadline`).
    pub(super) pending: HashMap<u64, (FollowUp, Instant, Duration)>,
    /// The `req` of the last tagged request; the attach rides out as 0.
    pub(super) last_req: u64,
}

impl App {
    /// Tag `request` with a fresh `req` and note what its reply completes, so
    /// that reply alone — a refusal included — completes or cancels it.
    pub(super) fn follow(&mut self, request: &Request, follow_up: FollowUp) -> WireFrame {
        self.follow_ups.last_req += 1;
        let deadline = crate::client::deadline(request).unwrap_or(Duration::MAX);
        self.follow_ups.pending.insert(
            self.follow_ups.last_req,
            (follow_up, Instant::now(), deadline),
        );
        tagged(request, self.follow_ups.last_req)
    }

    /// Give up on each follow-up whose daemon has not answered by its
    /// request's deadline, as on that request's refusal: its status says so,
    /// and a reply coming later completes nothing. Whether any was given up.
    pub fn expire_follow_ups(&mut self, now: Instant) -> bool {
        let overdue: Vec<(u64, Duration)> = self
            .follow_ups
            .pending
            .iter()
            .filter(|(_, (_, sent, deadline))| now.saturating_duration_since(*sent) >= *deadline)
            .map(|(req, (_, _, deadline))| (*req, *deadline))
            .collect();
        for &(req, deadline) in &overdue {
            let message = format!("tutti-server did not answer within {}s", deadline.as_secs());
            self.handle_response(Some(req), Response::Error { message });
        }
        !overdue.is_empty()
    }

    /// The follow-up a reply completes: the one its `req` keys — or, from an
    /// old daemon that echoes none, the oldest pending one that `fits`.
    pub(super) fn take_follow_up(
        &mut self,
        req: Option<u64>,
        fits: impl Fn(&FollowUp) -> bool,
    ) -> Option<FollowUp> {
        let req = match req {
            Some(req) => req,
            None if self.old_daemon => *self
                .follow_ups
                .pending
                .iter()
                .filter(|(_, (f, ..))| fits(f))
                .map(|(req, _)| req)
                .min()?,
            None => return None,
        };
        self.follow_ups
            .pending
            .remove(&req)
            .map(|(follow_up, ..)| follow_up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach::app::Mode;
    use crate::attach::fixtures::*;
    use crate::attach::sidebar;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// What request correlation fixes: a guarded answer refused while a merge
    /// runs used to cancel the merge's cleanup offer along with it. Now only
    /// the merge's own refusal cancels it.
    #[test]
    fn a_refused_answer_leaves_a_running_merge_its_cleanup_offer() {
        let mut app = App::new();
        let mut view = view_two_workspaces();
        view[1].parent = Some(WorkspaceId(1)); // web, nested under api
        attach_with(&mut app, view);
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('m'));
        app.on_key(plain('y')); // the merge rides out as req 1

        // While it runs, pane 3's prompt is answered from the peek, and the
        // daemon refuses the answer, naming no request of ours.
        let waiting = app
            .sidebar()
            .index_of(sidebar::EntryIdent::Waiting(PaneId(3), None))
            .unwrap();
        for _ in app.sidebar_selected()..waiting {
            app.on_key(plain('j'));
        }
        app.on_key(plain('p'));
        assert_eq!(
            app.on_key(plain('1')).len(),
            1,
            "the guarded answer goes out"
        );
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Error {
                message: "the prompt changed since you looked".into(),
            })
            .unwrap(),
        ));
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("prompt changed"))
        );

        let merged = || Response::Merged {
            pushed: false,
            bookmark: "main".into(),
        };
        app.handle_frame(reply(1, merged()));
        assert_eq!(app.mode, Mode::ConfirmCleanup(WorkspaceId(2)));

        // A merge refused itself loses its offer: a stray `Merged` for it then
        // raises nothing.
        app.on_key(plain('n'));
        app.on_key(plain('m'));
        app.on_key(plain('y')); // req 2
        app.handle_frame(reply(
            2,
            Response::Error {
                message: "the merge would conflict".into(),
            },
        ));
        app.handle_frame(reply(2, merged()));
        assert_eq!(app.mode, Mode::Sidebar);
    }

    /// A follow-up its daemon leaves unanswered past its request's deadline
    /// (the CLI's: three minutes for a merge) is given up on, saying so: a
    /// `Merged` landing later offers no cleanup.
    #[test]
    fn a_follow_up_the_daemon_leaves_unanswered_times_out() {
        let mut app = App::new();
        let mut view = view_two_workspaces();
        view[1].parent = Some(WorkspaceId(1));
        attach_with(&mut app, view);
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('m'));
        app.on_key(plain('y')); // the merge rides out as req 1

        let sent = Instant::now();
        assert!(!app.expire_follow_ups(sent + Duration::from_secs(179)));
        assert!(app.expire_follow_ups(sent + Duration::from_secs(181)));
        assert!(
            app.transient()
                .is_some_and(|t| t.contains("did not answer within 180s")),
            "{:?}",
            app.transient()
        );
        assert!(!app.expire_follow_ups(sent + Duration::from_secs(400)));

        app.handle_frame(reply(
            1,
            Response::Merged {
                pushed: false,
                bookmark: "main".into(),
            },
        ));
        assert_eq!(app.mode, Mode::Sidebar, "a late answer completes nothing");
    }

    /// Only add-project's own refusal disarms its jump to the new tab.
    #[test]
    fn only_its_own_refusal_disarms_add_projects_jump() {
        let root = temp_tree(&[]);
        let mut app = App::new();
        app.start_first_run_prompt(root.display().to_string());
        attach_with(&mut app, vec![]);
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // req 1
        let _ = std::fs::remove_dir_all(&root);
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Error {
                message: "no pane 9".into(),
            })
            .unwrap(),
        ));
        assert!(app.adopt_active_view, "another request's refusal");
        app.handle_frame(reply(
            1,
            Response::Error {
                message: "directory does not exist".into(),
            },
        ));
        assert!(!app.adopt_active_view, "its own");
    }

    /// An old daemon echoes no `req`: a reply completes the oldest follow-up
    /// of its kind, and any refusal cancels them all — the rule before tags.
    #[test]
    fn an_old_daemons_refusal_cancels_every_follow_up() {
        let mut app = App::new();
        let mut view = view_two_workspaces();
        view[1].parent = Some(WorkspaceId(1));
        app.handle_frame(WireFrame::Control(
            serde_json::to_vec(&Response::Attached {
                wire_rev: 7,
                session: "t".into(),
                workspaces: view,
                restorable: None,
                notes: Vec::new(),
            })
            .unwrap(),
        ));
        let untagged =
            |response: &Response| WireFrame::Control(serde_json::to_vec(response).unwrap());
        let merged = Response::Merged {
            pushed: false,
            bookmark: "main".into(),
        };
        focus_sidebar(&mut app);
        app.on_key(plain('j'));
        app.on_key(plain('m'));
        app.on_key(plain('y'));
        app.handle_frame(untagged(&merged));
        assert_eq!(app.mode, Mode::ConfirmCleanup(WorkspaceId(2)));

        app.on_key(plain('n'));
        app.on_key(plain('m'));
        app.on_key(plain('y'));
        app.handle_frame(untagged(&Response::Error {
            message: "the prompt changed since you looked".into(),
        }));
        app.handle_frame(untagged(&merged));
        assert_eq!(app.mode, Mode::Sidebar, "the refusal cancelled the offer");
    }
}
