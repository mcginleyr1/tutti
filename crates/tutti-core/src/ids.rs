use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(WorkspaceId);
id_type!(TabId);
id_type!(PaneId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_shows_inner() {
        assert_eq!(PaneId(7).to_string(), "7");
        assert_eq!(TabId(0).to_string(), "0");
    }

    #[test]
    fn serde_is_transparent() {
        assert_eq!(serde_json::to_string(&WorkspaceId(42)).unwrap(), "42");
        let id: PaneId = serde_json::from_str("9").unwrap();
        assert_eq!(id, PaneId(9));
    }
}
