//! What the two halves of a sandbox pane agree on: the client wrapper that
//! provisions the sandbox and tells it where to push, and the daemon that
//! pulls the pushed work back into a workspace.

/// The branch a sandbox's agent pushes its work to — `TUTTI_BRANCH` inside
/// the sandbox — and `SandboxPull` fetches: `tutti/<sandbox name>`.
pub fn sandbox_branch(name: &str) -> String {
    format!("tutti/{name}")
}

/// The git remote a sandbox clones and pushes to, as `(name, url)`, from `jj
/// git remote list` output (`<name> <url>` lines): `origin`, else the first.
pub fn pick_remote(listing: &str) -> Option<(&str, &str)> {
    let remotes: Vec<(&str, &str)> = listing
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, url)| (name, url.trim()))
        .collect();
    remotes
        .iter()
        .find(|(name, _)| *name == "origin")
        .or(remotes.first())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_remote_prefers_origin() {
        assert_eq!(
            pick_remote("upstream https://u.example/r.git\norigin git@github.com:me/r.git\n"),
            Some(("origin", "git@github.com:me/r.git"))
        );
        assert_eq!(
            pick_remote("upstream https://u.example/r.git\n"),
            Some(("upstream", "https://u.example/r.git"))
        );
        assert_eq!(pick_remote(""), None);
    }

    #[test]
    fn a_sandbox_pushes_under_tutti() {
        assert_eq!(sandbox_branch("tutti-p3-18f2a"), "tutti/tutti-p3-18f2a");
    }
}
