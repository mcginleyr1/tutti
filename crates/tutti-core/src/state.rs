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
}
