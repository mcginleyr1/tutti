use serde::Deserialize;

/// The wire's revision. RAISE IT on ANY change to the `Request`, `Response`
/// or `Event` shapes, additive fields included: the daemon's answer to an
/// attach names it, and binaries upgrade on install while running daemons do
/// not.
pub const WIRE_REV: u32 = 20;

/// The oldest peer revision this build still works with faithfully. A change
/// an older peer can ignore — a new optional field, a new request, a new
/// event — leaves it where it is; one an older peer would misread raises it
/// to the new `WIRE_REV` (`docs/transport-decision.md`, "Revisions and
/// floors").
pub const WIRE_FLOOR: u32 = 19;
const _: () = assert!(WIRE_FLOOR <= WIRE_REV);

/// One end's wire: the revision it speaks, and its floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wire {
    pub rev: u32,
    pub floor: u32,
}

/// This build's.
pub const WIRE: Wire = Wire {
    rev: WIRE_REV,
    floor: WIRE_FLOOR,
};

impl Wire {
    /// Whether a client on this wire and the daemon whose answer to an attach
    /// named `rev` and `floor` work together: each is at or above the other's
    /// floor. A daemon that names no floor is from before floors, when a
    /// client took no revision but its own: its floor is its revision. The
    /// refusal names both ends and says which to update, `client` being what
    /// the client calls itself.
    pub fn accepts(self, client: &str, rev: u32, floor: Option<u32>) -> Result<(), String> {
        let floor = floor.unwrap_or(rev);
        let update = if rev < self.floor {
            "update tutti and restart its daemon (`tutti server stop`)".to_string()
        } else if self.rev < floor {
            format!("update {client}")
        } else {
            return Ok(());
        };
        Err(format!(
            "tutti's daemon speaks wire revision {rev} (floor {floor}), {client} {} (floor {}): {update}",
            self.rev, self.floor
        ))
    }
}

/// The revision and the floor a daemon's answer to an attach names
/// (`Response::Attached`), read from the control frame `json` on their own:
/// a client too old to read the rest of the answer still reads why. `None`
/// for any other frame.
pub fn attached_wire(json: &[u8]) -> Option<(u32, Option<u32>)> {
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Answer {
        Attached {
            #[serde(default)]
            wire_rev: u32,
            #[serde(default)]
            wire_floor: Option<u32>,
        },
    }
    let Answer::Attached {
        wire_rev,
        wire_floor,
    } = serde_json::from_slice(json).ok()?;
    Some((wire_rev, wire_floor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, PaneId, Response, Tagged};

    /// The whole decision: a client at `C` with floor `Cf` takes a daemon at
    /// `D` with floor `Df` exactly when `Df <= C` and `Cf <= D`.
    #[test]
    fn a_client_takes_a_daemon_when_each_is_at_or_above_the_others_floor() {
        const DAEMON: Option<&str> = Some("update tutti and restart its daemon");
        const CLIENT: Option<&str> = Some("update rtty");
        // (C, Cf, D, Df, which end to update)
        let table = [
            // The same build, and each end one behind the other within the floors.
            (20, 19, 20, Some(19), None),
            (20, 19, 19, Some(19), None),
            (19, 19, 20, Some(19), None),
            // A daemon from before floors: its floor is its revision.
            (20, 19, 19, None, None),
            (20, 19, 18, None, DAEMON),
            (20, 19, 21, None, CLIENT),
            (20, 19, 0, None, DAEMON),
            (20, 20, 20, None, None),
            // The daemon is older than the client's floor.
            (20, 20, 19, Some(19), DAEMON),
            (23, 22, 20, Some(19), DAEMON),
            (23, 22, 21, Some(19), DAEMON),
            (23, 22, 22, Some(19), None),
            // The client is older than the daemon's floor.
            (20, 19, 23, Some(21), CLIENT),
            (18, 18, 20, Some(19), CLIENT),
            (20, 19, 23, Some(20), None),
            (20, 19, 23, Some(19), None),
            // Each end newer than the other's floor, far apart.
            (30, 5, 7, Some(2), None),
            (7, 2, 30, Some(5), None),
        ];
        for (rev, floor, daemon, daemon_floor, update) in table {
            let verdict = Wire { rev, floor }.accepts("rtty", daemon, daemon_floor);
            let exact = daemon_floor.unwrap_or(daemon) <= rev && floor <= daemon;
            assert_eq!(
                verdict.is_ok(),
                exact,
                "{rev}/{floor} with {daemon}/{daemon_floor:?}"
            );
            match (verdict, update) {
                (Ok(()), None) => {}
                (Err(refusal), Some(update)) => assert!(refusal.contains(update), "{refusal}"),
                (verdict, _) => panic!("{rev}/{floor} with {daemon}/{daemon_floor:?}: {verdict:?}"),
            }
        }
    }

    #[test]
    fn a_refusal_names_both_ends_and_the_one_to_update() {
        let client = Wire { rev: 20, floor: 19 };
        assert_eq!(
            client.accepts("rtty", 18, None).unwrap_err(),
            "tutti's daemon speaks wire revision 18 (floor 18), rtty 20 (floor 19): \
             update tutti and restart its daemon (`tutti server stop`)"
        );
        assert_eq!(
            client.accepts("rtty", 23, Some(21)).unwrap_err(),
            "tutti's daemon speaks wire revision 23 (floor 21), rtty 20 (floor 19): update rtty"
        );
    }

    /// The answer's revision and floor read whatever else it holds: as this
    /// build writes it, from a daemon before floors or before revisions, and
    /// from one so new that the rest of it no longer reads here.
    #[test]
    fn an_answers_wire_reads_on_its_own() {
        let attached = Tagged {
            req: Some(1),
            body: Response::Attached {
                session: "s".into(),
                workspaces: Vec::new(),
                wire_rev: WIRE_REV,
                wire_floor: Some(WIRE_FLOOR),
                restorable: None,
                notes: Vec::new(),
            },
        };
        let json = serde_json::to_vec(&attached).unwrap();
        assert_eq!(attached_wire(&json), Some((WIRE_REV, Some(WIRE_FLOOR))));
        let read = |json: &str| attached_wire(json.as_bytes());
        assert_eq!(
            read(r#"{"type":"attached","session":"s","workspaces":[],"wire_rev":19}"#),
            Some((19, None))
        );
        assert_eq!(
            read(r#"{"type":"attached","session":"s","workspaces":[]}"#),
            Some((0, None))
        );
        let newer = r#"{"type":"attached","session":"s","wire_rev":31,"wire_floor":30,
            "workspaces":[{"id":1,"shape":"a newer daemon's"}]}"#;
        assert!(serde_json::from_str::<Response>(newer).is_err());
        assert_eq!(read(newer), Some((31, Some(30))));
        assert_eq!(read(r#"{"req":2,"type":"ok"}"#), None);
        let exited = Event::PaneExited {
            pane: PaneId(1),
            code: 0,
        };
        assert_eq!(attached_wire(&serde_json::to_vec(&exited).unwrap()), None);
        assert_eq!(read("not json"), None);
    }
}
