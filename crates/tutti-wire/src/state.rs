use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    #[default]
    Unknown,
    Working,
    Blocked,
    Done,
    Idle,
}

/// What the classifier concluded from a pane's latest output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Observation {
    Working,
    Blocked,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateEvent {
    Classified(Observation),
    Focused,
    Activity,
}

/// Where a pane's current state came from, ranked lowest first so the derived
/// `Ord` is the precedence. Hook, Program (OSC 7501) and Progress (OSC 9;4) are
/// *claims*: each holds the state until its own source ends it, and nothing
/// ranked below may move it meanwhile. Screen (the classifier) and Activity
/// (output flow) never claim — the classify pass orders those two itself
/// (activity first, so a screen match wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    Activity,
    Screen,
    Progress,
    Program,
    Hook,
}

impl StateSource {
    /// Whether this source holds the state until it ends its own claim.
    pub fn claims(self) -> bool {
        self >= StateSource::Progress
    }

    /// Whether `self` may move a pane `from → to` while `current` set its
    /// state. Only a live claim ranked above `self` refuses — except that a
    /// visible permission dialog (a Screen `Blocked`) beats a Program/Progress
    /// `Working` claim, which is likelier stale than right. Hooks stay absolute.
    pub fn may_move(self, current: Option<StateSource>, from: AgentState, to: AgentState) -> bool {
        match current {
            Some(held) if held.claims() && held > self => {
                self == StateSource::Screen
                    && held != StateSource::Hook
                    && from == AgentState::Working
                    && to == AgentState::Blocked
            }
            _ => true,
        }
    }
}

impl AgentState {
    pub fn apply(self, event: StateEvent) -> AgentState {
        use AgentState::*;
        use Observation as Obs;
        use StateEvent::{Activity, Classified, Focused};
        match (self, event) {
            (Unknown, Classified(Obs::Working)) => Working,
            (Working, Classified(Obs::Working)) => Working,
            (Blocked, Classified(Obs::Working)) => Working,
            (Done, Classified(Obs::Working)) => Working,
            (Idle, Classified(Obs::Working)) => Working,

            (Unknown, Classified(Obs::Blocked)) => Blocked,
            (Working, Classified(Obs::Blocked)) => Blocked,
            (Blocked, Classified(Obs::Blocked)) => Blocked,
            (Done, Classified(Obs::Blocked)) => Blocked,
            (Idle, Classified(Obs::Blocked)) => Blocked,

            (Unknown, Classified(Obs::Done)) => Done,
            (Working, Classified(Obs::Done)) => Done,
            (Blocked, Classified(Obs::Done)) => Done,
            (Done, Classified(Obs::Done)) => Done,
            (Idle, Classified(Obs::Done)) => Done,

            (Done, Focused) => Idle,
            (Unknown, Focused) => Unknown,
            (Working, Focused) => Working,
            (Blocked, Focused) => Blocked,
            (Idle, Focused) => Idle,

            (Done, Activity) => Working,
            (Idle, Activity) => Working,
            (Unknown, Activity) => Unknown,
            (Working, Activity) => Working,
            (Blocked, Activity) => Blocked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::*;
    use Observation as Obs;
    use StateEvent::{Activity, Classified, Focused};

    #[test]
    fn transition_table() {
        let cases: &[(AgentState, StateEvent, AgentState)] = &[
            (Unknown, Classified(Obs::Working), Working),
            (Working, Classified(Obs::Working), Working),
            (Blocked, Classified(Obs::Working), Working),
            (Done, Classified(Obs::Working), Working),
            (Idle, Classified(Obs::Working), Working),
            (Unknown, Classified(Obs::Blocked), Blocked),
            (Working, Classified(Obs::Blocked), Blocked),
            (Blocked, Classified(Obs::Blocked), Blocked),
            (Done, Classified(Obs::Blocked), Blocked),
            (Idle, Classified(Obs::Blocked), Blocked),
            (Unknown, Classified(Obs::Done), Done),
            (Working, Classified(Obs::Done), Done),
            (Blocked, Classified(Obs::Done), Done),
            (Done, Classified(Obs::Done), Done),
            (Idle, Classified(Obs::Done), Done),
            (Unknown, Focused, Unknown),
            (Working, Focused, Working),
            (Blocked, Focused, Blocked),
            (Done, Focused, Idle),
            (Idle, Focused, Idle),
            (Unknown, Activity, Unknown),
            (Working, Activity, Working),
            (Blocked, Activity, Blocked),
            (Done, Activity, Working),
            (Idle, Activity, Working),
        ];
        for &(from, event, expected) in cases {
            assert_eq!(from.apply(event), expected, "from {from:?} on {event:?}");
        }
    }

    #[test]
    fn default_is_unknown() {
        assert_eq!(AgentState::default(), Unknown);
    }

    #[test]
    fn source_precedence_table() {
        use StateSource::*;
        let cases: &[(
            StateSource,
            Option<StateSource>,
            AgentState,
            AgentState,
            bool,
        )] = &[
            // Nothing (or a non-claiming source) holds the state: anyone moves it.
            (Activity, None, Done, Working, true),
            (Screen, None, Idle, Done, true),
            (Progress, None, Unknown, Working, true),
            (Activity, Some(Screen), Done, Working, true),
            (Activity, Some(Activity), Idle, Working, true),
            // A claim refuses every source ranked below it.
            (Activity, Some(Progress), Done, Working, false),
            (Screen, Some(Progress), Working, Done, false),
            (Screen, Some(Program), Done, Working, false),
            (Progress, Some(Program), Done, Working, false),
            (Program, Some(Hook), Working, Done, false),
            (Screen, Some(Hook), Working, Done, false),
            // ...and yields to itself and to anything ranked above.
            (Progress, Some(Progress), Working, Done, true),
            (Program, Some(Progress), Working, Done, true),
            (Hook, Some(Program), Working, Blocked, true),
            (Hook, Some(Hook), Blocked, Working, true),
            // A visible permission dialog beats a stale Program/Progress Working…
            (Screen, Some(Progress), Working, Blocked, true),
            (Screen, Some(Program), Working, Blocked, true),
            // …but only from Working, only for Blocked, and never over a hook.
            (Screen, Some(Program), Done, Blocked, false),
            (Screen, Some(Program), Idle, Blocked, false),
            (Activity, Some(Program), Working, Blocked, false),
            (Screen, Some(Hook), Working, Blocked, false),
        ];
        for &(source, current, from, to, expected) in cases {
            assert_eq!(
                source.may_move(current, from, to),
                expected,
                "{source:?} moving {from:?}→{to:?} over {current:?}"
            );
        }
    }

    #[test]
    fn only_hook_program_and_progress_claim() {
        use StateSource::*;
        assert!(Hook.claims() && Program.claims() && Progress.claims());
        assert!(!Screen.claims() && !Activity.claims());
        assert!(Hook > Program && Program > Progress && Progress > Screen && Screen > Activity);
    }
}
