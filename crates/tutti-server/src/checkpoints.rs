//! Per-turn checkpoints: one pane's in-memory list of the jj `@` commits its
//! workspace held each time an agent turn ended. Recording rides the change
//! probe — the probe's own snapshot of `@` is the checkpoint, so tutti never
//! takes a snapshot just for this. Pure bookkeeping; the server drives the jj
//! side. Not persisted: a restarted daemon starts every pane's list empty.

use tutti_core::{AgentState, Checkpoint, FileStat};

/// The most checkpoints kept per pane; the oldest drops first.
pub const KEEP: usize = 50;

/// One pane's checkpoints plus what the next change probe must record for it.
#[derive(Default)]
pub struct Turns {
    entries: Vec<Checkpoint>,
    /// The last `n` handed out, so numbers are never reused after a drop.
    last: u64,
    /// The commit the next turn diffs from: the newest checkpoint, or the
    /// working copy a restore left behind. `None` before anything is recorded.
    head: Option<String>,
    /// Labels noted since the last probe drained them, oldest first.
    pending: Vec<String>,
}

impl Turns {
    /// Note a state transition: leaving `Working` ends a turn (labelled with the
    /// state it ended in), and entering it with nothing recorded yet takes the
    /// baseline turn 1 diffs against.
    pub fn note_transition(&mut self, from: AgentState, to: AgentState) {
        if to == AgentState::Working && self.head.is_none() {
            self.note_start();
        }
        if ends_turn(from, to) {
            self.pending.push(format!("{to:?}").to_lowercase());
        }
    }

    /// Note that an agent just started in the pane: where it began becomes a
    /// checkpoint, so its first turn has a "before" to diff and restore to.
    pub fn note_start(&mut self) {
        self.pending.push("start".into());
    }

    /// Hand the noted labels to a probe about to snapshot, which records them.
    pub fn take_pending(&mut self) -> Vec<String> {
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
        assert_eq!(turns.take_pending(), vec!["start", "blocked"]);
        assert!(turns.take_pending().is_empty(), "taking drains");

        turns.record(None, "c1", "start", 1, &[]).unwrap();
        turns.note_transition(Blocked, Working);
        turns.note_transition(Working, Done);
        assert_eq!(
            turns.take_pending(),
            vec!["done"],
            "a pane with a head takes no second baseline"
        );
    }

    #[test]
    fn record_numbers_monotonically_and_chains_from_the_head() {
        let mut turns = Turns::default();
        assert_eq!(turns.record(None, "c1", "start", 10, &[]), Some(1));
        assert_eq!(
            turns.record(
                Some("c1".into()),
                "c2",
                "done",
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
        turns.record(None, "c1", "start", 1, &[]).unwrap();
        assert_eq!(
            turns.record(Some("c1".into()), "c1", "done", 2, &[]),
            None,
            "a turn that changed nothing gets no checkpoint"
        );
        assert_eq!(
            turns.record(Some("c0".into()), "c2", "done", 2, &[stat(1, 0)]),
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
            turns.record(head.clone(), &commit, "done", i as u64, &[stat(1, 0)]);
            head = Some(commit);
        }
        assert_eq!(turns.entries().len(), KEEP);
        assert_eq!(turns.entries()[0].n, 6, "the five oldest dropped");
        assert!(turns.get(5).is_none());
        let next = turns.record(head, "fresh", "done", 99, &[stat(1, 0)]);
        assert_eq!(next, Some(KEEP as u64 + 6));
    }

    #[test]
    fn a_restore_head_is_the_next_turns_from() {
        let mut turns = Turns::default();
        turns.record(None, "c1", "start", 1, &[]).unwrap();
        turns.set_head("restored".into());
        assert_eq!(turns.record(Some("c1".into()), "c2", "done", 2, &[]), None);
        assert_eq!(
            turns.record(Some("restored".into()), "c2", "done", 2, &[stat(1, 0)]),
            Some(2)
        );
    }
}
