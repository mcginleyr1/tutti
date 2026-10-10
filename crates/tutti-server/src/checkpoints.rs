//! Per-turn checkpoints: one pane's in-memory list of the jj `@` commits its
//! workspace held each time an agent turn ended. Recording rides the change
//! probe — the probe's own snapshot of `@` is the checkpoint, so tutti never
//! takes a snapshot just for this. Pure bookkeeping; the server drives the jj
//! side. Not persisted: a restarted daemon starts every pane's list empty.

use tutti_core::{AgentState, Checkpoint, FileStat};

/// The most checkpoints kept per pane; the oldest drops first.
pub const KEEP: usize = 50;

/// A checkpoint noted for the next change probe to record: its label, and the
/// prompt that started its turn when a hook reported one.
pub type Noted = (String, Option<String>);

/// One pane's checkpoints plus what the next change probe must record for it.
#[derive(Default)]
pub struct Turns {
    entries: Vec<Checkpoint>,
    /// The last `n` handed out, so numbers are never reused after a drop.
    last: u64,
    /// The commit the next turn diffs from: the newest checkpoint, or the
    /// working copy a restore left behind. `None` before anything is recorded.
    head: Option<String>,
    /// Checkpoints noted since the last probe drained them, oldest first.
    pending: Vec<Noted>,
    /// The prompt that started the turn in flight, as a hook reported it.
    prompt: Option<String>,
}

impl Turns {
    /// Note a state transition: leaving `Working` ends a turn (labelled with the
    /// state it ended in, and the prompt that started it), and entering it with
    /// nothing recorded yet takes the baseline turn 1 diffs against. A turn
    /// that blocked is still the prompt's — once answered the agent carries on
    /// with it — so only an end in any other state lets the prompt go.
    pub fn note_transition(&mut self, from: AgentState, to: AgentState) {
        if to == AgentState::Working && self.head.is_none() {
            self.note_start();
        }
        if ends_turn(from, to) {
            let label = format!("{to:?}").to_lowercase();
            let prompt = match to {
                AgentState::Blocked => self.prompt.clone(),
                _ => self.prompt.take(),
            };
            self.pending.push((label, prompt));
        }
    }

    /// Note the prompt that starts the next turn — replacing one whose turn
    /// never closed (an interrupted agent sends no stop) — or, with `None`,
    /// forget it.
    pub fn set_prompt(&mut self, prompt: Option<String>) {
        self.prompt = prompt;
    }

    /// Note that an agent just started in the pane: where it began becomes a
    /// checkpoint, so its first turn has a "before" to diff and restore to.
    pub fn note_start(&mut self) {
        self.pending.push(("start".into(), None));
    }

    /// Hand the noted checkpoints to a probe about to snapshot, which records
    /// them.
    pub fn take_pending(&mut self) -> Vec<Noted> {
        std::mem::take(&mut self.pending)
    }

    pub fn head(&self) -> Option<String> {
        self.head.clone()
    }

    /// Point the next turn's diff at `commit` without recording a checkpoint —
    /// what a restore leaves behind is a state the list already holds.
    pub fn set_head(&mut self, commit: String) {
        self.head = Some(commit);
    }

    pub fn entries(&self) -> &[Checkpoint] {
        &self.entries
    }

    pub fn get(&self, n: u64) -> Option<&Checkpoint> {
        self.entries.iter().find(|c| c.n == n)
    }

    /// Record `commit` as the next checkpoint, its stat (`files`) taken over
    /// `from..commit`. Refused — `None` — when `@` has not moved since the head
    /// (a turn that changed nothing gets no checkpoint), and when the head
    /// moved under the caller while it diffed (`from` is no longer the head; a
    /// concurrent probe already recorded a newer state).
    pub fn record(
        &mut self,
        from: Option<String>,
        commit: &str,
        label: &str,
        prompt: Option<&str>,
        time: u64,
        files: &[FileStat],
    ) -> Option<u64> {
        if self.head != from || from.as_deref() == Some(commit) {
            return None;
        }
        self.last += 1;
        self.entries.push(Checkpoint {
            n: self.last,
            commit: commit.to_string(),
            from,
            time,
            label: label.to_string(),
            prompt: prompt.map(str::to_string),
            files: files.len(),
            insertions: files.iter().map(|f| f.insertions).sum(),
            deletions: files.iter().map(|f| f.deletions).sum(),
        });
        if self.entries.len() > KEEP {
            self.entries.remove(0);
        }
        self.head = Some(commit.to_string());
        Some(self.last)
    }
}

/// A turn ends when a pane leaves `Working`, whatever it lands in.
fn ends_turn(from: AgentState, to: AgentState) -> bool {
    from == AgentState::Working && to != AgentState::Working
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::*;

    fn stat(insertions: u64, deletions: u64) -> FileStat {
        FileStat {
            path: "a.txt".into(),
            insertions,
            deletions,
        }
    }

    /// The labels of what `turns` noted, draining them.
    fn labels(turns: &mut Turns) -> Vec<String> {
        turns.take_pending().into_iter().map(|(l, _)| l).collect()
    }

    #[test]
    fn a_turn_ends_on_leaving_working_only() {
        for to in [Unknown, Blocked, Done, Idle] {
            assert!(ends_turn(Working, to), "Working → {to:?} ends a turn");
            assert!(!ends_turn(to, Working), "{to:?} → Working starts one");
        }
        assert!(!ends_turn(Working, Working));
        assert!(!ends_turn(Blocked, Done), "no turn was running");
        assert!(
            !ends_turn(Done, Idle),
            "focusing a finished pane is not a turn"
        );
    }

    #[test]
    fn transitions_note_a_baseline_once_then_each_turn_end() {
        let mut turns = Turns::default();
        turns.note_transition(Unknown, Working);
        turns.note_transition(Working, Blocked);
        assert_eq!(labels(&mut turns), vec!["start", "blocked"]);
        assert!(turns.take_pending().is_empty(), "taking drains");

        turns.record(None, "c1", "start", None, 1, &[]).unwrap();
        turns.note_transition(Blocked, Working);
        turns.note_transition(Working, Done);
        assert_eq!(
            labels(&mut turns),
            vec!["done"],
            "a pane with a head takes no second baseline"
        );
    }

    #[test]
    fn a_prompt_rides_each_end_of_its_turn_until_the_turn_finishes() {
        let mut turns = Turns::default();
        turns.set_prompt(Some("fix the sidebar".into()));
        turns.note_transition(Idle, Working);
        assert_eq!(
            turns.take_pending(),
            vec![("start".to_string(), None)],
            "the baseline predates the prompt"
        );
        turns.record(None, "c1", "start", None, 1, &[]).unwrap();

        let prompt = Some("fix the sidebar".to_string());
        turns.note_transition(Working, Blocked);
        turns.note_transition(Blocked, Working);
        turns.note_transition(Working, Done);
        assert_eq!(
            turns.take_pending(),
            vec![
                ("blocked".to_string(), prompt.clone()),
                ("done".to_string(), prompt)
            ],
            "once answered, a blocked agent carries on with the same prompt"
        );
        turns.note_transition(Done, Working);
        turns.note_transition(Working, Done);
        assert_eq!(
            turns.take_pending(),
            vec![("done".to_string(), None)],
            "a turn no prompt started has none"
        );
    }

    #[test]
    fn a_recorded_checkpoint_keeps_its_prompt() {
        let mut turns = Turns::default();
        turns.record(None, "c1", "start", None, 1, &[]).unwrap();
        let fix = Some("fix the sidebar");
        turns.record(Some("c1".into()), "c2", "done", fix, 2, &[stat(1, 0)]);
        assert_eq!(turns.get(1).unwrap().prompt, None);
        assert_eq!(turns.get(2).unwrap().prompt.as_deref(), fix);
    }

    #[test]
    fn record_numbers_monotonically_and_chains_from_the_head() {
        let mut turns = Turns::default();
        assert_eq!(turns.record(None, "c1", "start", None, 10, &[]), Some(1));
        assert_eq!(
            turns.record(
                Some("c1".into()),
                "c2",
                "done",
                None,
                20,
                &[stat(3, 1), stat(1, 0)]
            ),
            Some(2)
        );
        let second = turns.get(2).unwrap();
        assert_eq!(second.from.as_deref(), Some("c1"));
        assert_eq!(
            (second.files, second.insertions, second.deletions),
            (2, 4, 1)
        );
        assert_eq!(turns.head().as_deref(), Some("c2"));
    }

    #[test]
    fn record_dedupes_an_unmoved_head_and_refuses_a_stale_from() {
        let mut turns = Turns::default();
        turns.record(None, "c1", "start", None, 1, &[]).unwrap();
        assert_eq!(
            turns.record(Some("c1".into()), "c1", "done", None, 2, &[]),
            None,
            "a turn that changed nothing gets no checkpoint"
        );
        assert_eq!(
            turns.record(Some("c0".into()), "c2", "done", None, 2, &[stat(1, 0)]),
            None,
            "a diff taken against a superseded head is dropped"
        );
        assert_eq!(turns.entries().len(), 1);
    }

    #[test]
    fn the_cap_drops_the_oldest_and_never_reuses_a_number() {
        let mut turns = Turns::default();
        let mut head = None;
        for i in 0..KEEP + 5 {
            let commit = format!("c{i}");
            turns.record(head.clone(), &commit, "done", None, i as u64, &[stat(1, 0)]);
            head = Some(commit);
        }
        assert_eq!(turns.entries().len(), KEEP);
        assert_eq!(turns.entries()[0].n, 6, "the five oldest dropped");
        assert!(turns.get(5).is_none());
        let next = turns.record(head, "fresh", "done", None, 99, &[stat(1, 0)]);
        assert_eq!(next, Some(KEEP as u64 + 6));
    }

    #[test]
    fn a_restore_head_is_the_next_turns_from() {
        let mut turns = Turns::default();
        turns.record(None, "c1", "start", None, 1, &[]).unwrap();
        turns.set_head("restored".into());
        assert_eq!(
            turns.record(Some("c1".into()), "c2", "done", None, 2, &[]),
            None
        );
        assert_eq!(
            turns.record(
                Some("restored".into()),
                "c2",
                "done",
                None,
                2,
                &[stat(1, 0)]
            ),
            Some(2)
        );
    }
}
