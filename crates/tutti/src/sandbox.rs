//! k8s sandboxes: a `[[sandboxes]]` profile names somewhere an agent can run
//! instead of locally, and `tutti sandbox run <profile> -- <cmd>` is what a
//! sandbox pane runs. It provisions a kubernetes-sigs/agent-sandbox `Sandbox`
//! (from the profile's image) or `SandboxClaim` (from its warm pool) through
//! `kubectl`, waits for it to be Ready, then `kubectl exec -it`s `cmd` in its
//! pod — so the pane's pty, and with it the screen classifier, the badges and
//! answering from the queue, stays local. The sandbox owns the code: its image
//! or template clones the repo and pushes the agent's work (an image sandbox,
//! or a claim with `inject_env`, is told the workspace's git remote as
//! `TUTTI_GIT_REMOTE` and the branch to push to as `TUTTI_BRANCH`, which
//! `SandboxPull` fetches back). However the wrapper ends — exit, failure,
//! ctrl-c, or the SIGKILL a killed pane's process group gets — a watchdog
//! deletes what it made, and a `shutdownTime` the wrapper keeps pushing ahead
//! reaps the sandbox should even that fail.

use std::io::{PipeWriter, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// How often readiness is polled.
const POLL: Duration = Duration::from_secs(1);
/// How long a sandbox may take to become Ready — image pulls included.
const READY_TIMEOUT: Duration = Duration::from_secs(600);

/// One configured place to run agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// The kubectl context; `None` uses the current one.
    pub context: Option<String>,
    /// `None` uses the context's namespace.
    pub namespace: Option<String>,
    pub source: Source,
    /// The container `cmd` runs in; `None` lets kubectl pick the pod's default.
    pub container: Option<String>,
    /// Whether a warm-pool claim carries the env an image sandbox gets. The
    /// pool's template must allow it (`envVarsInjectionPolicy: Allowed`); its
    /// default rejects the claim. It costs the pool's head start: a running
    /// pod cannot take env, so the claim gets a fresh one. Never set on an
    /// image profile.
    pub inject_env: bool,
    /// How long a sandbox outlives its last renewal. The wrapper renews every
    /// third of it, so this bounds how long an orphan lingers after a crash.
    pub ttl: Duration,
}

/// What a profile provisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A `Sandbox` whose one container, `agent`, runs this image. Its
    /// entrypoint must keep running (clone the repo, then sleep) for the exec.
    Image(String),
    /// A `SandboxClaim` on this `SandboxWarmPool`.
    WarmPool(String),
}

impl Source {
    /// The kubectl resource provisioned, fully qualified so that no other CRD
    /// named `sandbox` can shadow it.
    fn resource(&self) -> &'static str {
        match self {
            Source::Image(_) => "sandboxes.agents.x-k8s.io",
            Source::WarmPool(_) => "sandboxclaims.extensions.agents.x-k8s.io",
        }
    }
}

/// What a sandbox pane runs: provision sandbox `name` (the daemon names it per
/// pane), exec `cmd`, clean up. Returns the exit code to leave with — the
/// remote command's own.
pub fn run(profile: &Profile, name: &str, cmd: &[String]) -> Result<i32> {
    let remote = (matches!(profile.source, Source::Image(_)) || profile.inject_env)
        .then(git_remote)
        .flatten();
    let object = manifest(profile, name, &expiry(profile.ttl), remote.as_deref());
    say(&format!(
        "creating {} {name} on {}{}",
        object["kind"].as_str().unwrap_or_default(),
        profile.context.as_deref().unwrap_or("the current context"),
        profile
            .namespace
            .as_deref()
            .map_or(String::new(), |ns| format!("/{ns}")),
    ));
    let _reaper = reap_on_exit(profile, name)?;
    capture(
        kubectl(profile).args(["create", "-f", "-"]),
        Some(object.to_string().as_bytes()),
    )?;
    say("waiting for it to be ready (ctrl-c cancels)");
    let pod = await_ready(profile, name)?;
    say(&format!("ready — exec {} in pod {pod}", cmd.join(" ")));
    keep_renewing(profile.clone(), name.into());
    let mut exec = kubectl(profile);
    exec.args(["exec", "-it", &pod]);
    if let Some(container) = &profile.container {
        exec.args(["-c", container]);
    }
    let status = exec
        .arg("--")
        .arg("env")
        .args(terminal_env())
        .args(cmd)
        .status()
        .context("running kubectl exec")?;
    Ok(status.code().unwrap_or(1))
}

/// Arm the delete of sandbox `name` for however this process ends. A watchdog
/// blocks reading a pipe only this process holds open (std pipes are
/// close-on-exec, so no child inherits it) and deletes once the pipe closes —
/// on exit, on a crash, or on the SIGKILL tutti sends a killed pane's whole
/// process group. Its own process group keeps it out of that SIGKILL and of
/// the pane's hangup. Dropping the guard closes the pipe early.
fn reap_on_exit(profile: &Profile, name: &str) -> Result<Reaper> {
    let (reader, writer) = std::io::pipe().context("creating the reaper pipe")?;
    let mut delete = kubectl(profile);
    delete.args([
        "delete",
        profile.source.resource(),
        name,
        "--wait=false",
        "--ignore-not-found",
    ]);
    // Never waited on: it outlives this process by design.
    Command::new("sh")
        .args(["-c", "read _; exec \"$@\"", "sh"])
        .arg(delete.get_program())
        .args(delete.get_args())
        .stdin(reader)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("starting the sandbox reaper")?;
    Ok(Reaper {
        name: name.into(),
        _pipe: writer,
    })
}

/// The open end of the reaper's pipe: dropping it sets the delete off.
struct Reaper {
    name: String,
    _pipe: PipeWriter,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        say(&format!("deleting {}", self.name));
    }
}

/// Poll until Ready, echoing each new status message, and answer the pod to
/// exec into. A claim whose env its template refuses fails at once.
fn await_ready(profile: &Profile, name: &str) -> Result<String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last = String::new();
    loop {
        let json = capture(
            kubectl(profile).args(["get", profile.source.resource(), name, "-o", "json"]),
            None,
        )?;
        let object: Value = serde_json::from_str(&json).context("parsing kubectl get")?;
        match readiness(&object) {
            Readiness::Ready(pod) => return Ok(pod),
            Readiness::Rejected(message) => bail!(
                "{message} — set `envVarsInjectionPolicy: Allowed` on the pool's template, or drop `inject_env`"
            ),
            Readiness::Waiting(message) if message != last => {
                say(&format!("  {message}"));
                last = message;
            }
            Readiness::Waiting(_) => {}
        }
        if Instant::now() >= deadline {
            bail!("not ready after {}s: {last}", READY_TIMEOUT.as_secs());
        }
        std::thread::sleep(POLL);
    }
}

/// Push the expiry a full TTL ahead every third of it, on a thread that ends
/// with the process. A failed renewal is dropped: two more tries come before
/// the TTL runs out, and the agent's session matters more.
fn keep_renewing(profile: Profile, name: String) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(profile.ttl / 3);
            let patch = renewal(&profile.source, &expiry(profile.ttl));
            let _ = capture(
                kubectl(&profile).args([
                    "patch",
                    profile.source.resource(),
                    &name,
                    "--type",
                    "merge",
                    "-p",
                    &patch.to_string(),
                ]),
                None,
            );
        }
    });
}

/// The object `run` creates: a `Sandbox` running the image as container
/// `agent`, or a `SandboxClaim` on the warm pool. Either is labelled tutti's,
/// expires at `shutdown`, and is deleted outright when it does. The sandbox is
/// told the branch to push its work to (`TUTTI_BRANCH`) and, when there is
/// one, the workspace's git remote (`TUTTI_GIT_REMOTE`) — a claim only with
/// `inject_env`, since a template's default policy rejects injected env.
fn manifest(profile: &Profile, name: &str, shutdown: &str, remote: Option<&str>) -> Value {
    let metadata = json!({
        "name": name,
        "labels": {"app.kubernetes.io/managed-by": "tutti"},
    });
    let branch = tutti_core::sandbox_branch(name);
    let env = remote
        .map(|url| ("TUTTI_GIT_REMOTE", url))
        .into_iter()
        .chain([("TUTTI_BRANCH", branch.as_str())])
        .map(|(key, value)| json!({"name": key, "value": value}));
    match &profile.source {
        Source::Image(image) => json!({
            "apiVersion": "agents.x-k8s.io/v1beta1",
            "kind": "Sandbox",
            "metadata": metadata,
            "spec": {
                "shutdownTime": shutdown,
                "shutdownPolicy": "Delete",
                "podTemplate": {"spec": {"containers": [
                    {"name": "agent", "image": image, "env": env.collect::<Value>()},
                ]}},
            },
        }),
        Source::WarmPool(pool) => {
            let mut spec = json!({
                "warmPoolRef": {"name": pool},
                "lifecycle": {"shutdownTime": shutdown, "shutdownPolicy": "Delete"},
            });
            if profile.inject_env {
                spec["env"] = env
                    .map(|mut var| {
                        if let Some(container) = &profile.container {
                            var["containerName"] = json!(container);
                        }
                        var
                    })
                    .collect();
            }
            json!({
                "apiVersion": "extensions.agents.x-k8s.io/v1beta1",
                "kind": "SandboxClaim",
                "metadata": metadata,
                "spec": spec,
            })
        }
    }
}

/// The merge patch moving the expiry to `shutdown`.
fn renewal(source: &Source, shutdown: &str) -> Value {
    match source {
        Source::Image(_) => json!({"spec": {"shutdownTime": shutdown}}),
        Source::WarmPool(_) => json!({"spec": {"lifecycle": {"shutdownTime": shutdown}}}),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Readiness {
    /// Ready, with the pod to exec into.
    Ready(String),
    /// Not yet, with the Ready condition's message.
    Waiting(String),
    /// Never: the claim's template refuses the env it carries.
    Rejected(String),
}

/// Read a `kubectl get -o json` object's Ready condition. A Sandbox's pod
/// shares its name; a claim names the sandbox it bound, whose pod does.
fn readiness(object: &Value) -> Readiness {
    let ready = object["status"]["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["type"] == "Ready");
    let pod = match object["kind"].as_str() {
        Some("SandboxClaim") => object["status"]["sandbox"]["name"].as_str(),
        _ => object["metadata"]["name"].as_str(),
    };
    let message = ready
        .and_then(|c| c["message"].as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or("waiting for the controller")
        .into();
    match (ready.is_some_and(|c| c["status"] == "True"), pod) {
        (true, Some(pod)) => Readiness::Ready(pod.into()),
        _ if ready.is_some_and(|c| c["reason"] == "EnvVarsInjectionRejected") => {
            Readiness::Rejected(message)
        }
        _ => Readiness::Waiting(message),
    }
}

/// A TTL from now, as the RFC 3339 a k8s `metav1.Time` takes.
fn expiry(ttl: Duration) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    rfc3339(now + ttl.as_secs())
}

/// `secs` since the epoch as RFC 3339 UTC, e.g. `2025-10-09T08:53:20Z`. The
/// date is Howard Hinnant's civil-from-days.
fn rfc3339(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem / 60 % 60,
        rem % 60
    )
}

/// The workspace's git remote URL for the sandbox to clone: `origin`, else the
/// first. Read without snapshotting the working copy, so asking records no jj
/// operation for tutti's VCS watch to chase.
fn git_remote() -> Option<String> {
    let out = Command::new("jj")
        .args(["--ignore-working-copy", "git", "remote", "list"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    tutti_core::pick_remote(&String::from_utf8(out.stdout).ok()?).map(|(_, url)| url.to_string())
}

/// `TERM` (and `COLORTERM` when set) for the remote command — `kubectl exec`
/// forwards neither, and an agent without them renders monochrome.
fn terminal_env() -> Vec<String> {
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    std::iter::once(format!("TERM={term}"))
        .chain(
            std::env::var("COLORTERM")
                .ok()
                .map(|v| format!("COLORTERM={v}")),
        )
        .collect()
}

fn kubectl(profile: &Profile) -> Command {
    let mut cmd = Command::new("kubectl");
    if let Some(context) = &profile.context {
        cmd.args(["--context", context]);
    }
    if let Some(namespace) = &profile.namespace {
        cmd.args(["--namespace", namespace]);
    }
    cmd
}

/// Run a short kubectl call to completion, feeding it `stdin`; its stdout on
/// success, its stderr as the error otherwise.
fn capture(cmd: &mut Command, stdin: Option<&[u8]>) -> Result<String> {
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running kubectl (is it on PATH?)")?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(bytes).context("writing to kubectl")?;
    }
    let output = child.wait_with_output().context("waiting for kubectl")?;
    if !output.status.success() {
        bail!(
            "kubectl: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One line of progress on the pane. A failed write (a hung-up pty) is
/// dropped — nothing it narrates depends on it.
fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "tutti: {line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(source: Source) -> Profile {
        Profile {
            name: "orb".into(),
            context: Some("orbstack".into()),
            namespace: None,
            source,
            container: None,
            inject_env: false,
            ttl: Duration::from_secs(3_600),
        }
    }

    #[test]
    fn an_image_profile_makes_a_sandbox_told_the_git_remote_and_branch() {
        let object = manifest(
            &profile(Source::Image("ghcr.io/acme/agent:1".into())),
            "tutti-p3-ab",
            "2026-10-10T03:00:00Z",
            Some("git@github.com:acme/app.git"),
        );
        assert_eq!(object["apiVersion"], "agents.x-k8s.io/v1beta1");
        assert_eq!(object["kind"], "Sandbox");
        assert_eq!(object["metadata"]["name"], "tutti-p3-ab");
        assert_eq!(
            object["metadata"]["labels"]["app.kubernetes.io/managed-by"],
            "tutti"
        );
        assert_eq!(object["spec"]["shutdownTime"], "2026-10-10T03:00:00Z");
        assert_eq!(
            object["spec"]["shutdownPolicy"], "Delete",
            "an expired orphan is deleted, not kept as a husk"
        );
        let container = &object["spec"]["podTemplate"]["spec"]["containers"][0];
        assert_eq!(container["name"], "agent");
        assert_eq!(container["image"], "ghcr.io/acme/agent:1");
        assert_eq!(
            container["env"],
            json!([
                {"name": "TUTTI_GIT_REMOTE", "value": "git@github.com:acme/app.git"},
                {"name": "TUTTI_BRANCH", "value": "tutti/tutti-p3-ab"},
            ])
        );
        let bare = manifest(&profile(Source::Image("img".into())), "n", "t", None);
        assert_eq!(
            bare["spec"]["podTemplate"]["spec"]["containers"][0]["env"],
            json!([{"name": "TUTTI_BRANCH", "value": "tutti/n"}]),
            "no remote, just the branch to push to"
        );
    }

    #[test]
    fn a_warm_pool_profile_claims_from_the_pool_without_env() {
        let object = manifest(
            &profile(Source::WarmPool("claude-pool".into())),
            "tutti-p3-ab",
            "2026-10-10T03:00:00Z",
            Some("git@github.com:acme/app.git"),
        );
        assert_eq!(object["apiVersion"], "extensions.agents.x-k8s.io/v1beta1");
        assert_eq!(object["kind"], "SandboxClaim");
        assert_eq!(object["spec"]["warmPoolRef"]["name"], "claude-pool");
        assert_eq!(
            object["spec"]["lifecycle"],
            json!({"shutdownTime": "2026-10-10T03:00:00Z", "shutdownPolicy": "Delete"})
        );
        assert!(
            object["spec"].get("env").is_none(),
            "a template's default policy rejects a claim carrying env"
        );
    }

    #[test]
    fn a_claim_with_inject_env_carries_it_into_the_profiles_container() {
        let mut injecting = Profile {
            inject_env: true,
            container: Some("agent".into()),
            ..profile(Source::WarmPool("claude-pool".into()))
        };
        let object = manifest(
            &injecting,
            "tutti-p3-ab",
            "T",
            Some("https://g.example/a.git"),
        );
        assert_eq!(
            object["spec"]["env"],
            json!([
                {"name": "TUTTI_GIT_REMOTE", "value": "https://g.example/a.git", "containerName": "agent"},
                {"name": "TUTTI_BRANCH", "value": "tutti/tutti-p3-ab", "containerName": "agent"},
            ])
        );
        injecting.container = None;
        assert_eq!(
            manifest(&injecting, "n", "T", None)["spec"]["env"],
            json!([{"name": "TUTTI_BRANCH", "value": "tutti/n"}]),
            "without a container the controller picks the pod's"
        );
    }

    #[test]
    fn renewal_patches_the_expiry_where_each_kind_keeps_it() {
        assert_eq!(
            renewal(&Source::Image("i".into()), "T"),
            json!({"spec": {"shutdownTime": "T"}})
        );
        assert_eq!(
            renewal(&Source::WarmPool("p".into()), "T"),
            json!({"spec": {"lifecycle": {"shutdownTime": "T"}}})
        );
    }

    #[test]
    fn readiness_reads_the_ready_condition_and_the_pod() {
        let sandbox = json!({
            "kind": "Sandbox",
            "metadata": {"name": "tutti-p1-a"},
            "status": {"conditions": [
                {"type": "Suspended", "status": "False", "message": "Sandbox is not suspended"},
                {"type": "Ready", "status": "True", "message": "Pod is Ready"},
            ]},
        });
        assert_eq!(readiness(&sandbox), Readiness::Ready("tutti-p1-a".into()));

        let claim = json!({
            "kind": "SandboxClaim",
            "metadata": {"name": "tutti-p1-b"},
            "status": {
                "conditions": [{"type": "Ready", "status": "True", "message": "Pod is Ready"}],
                "sandbox": {"name": "claude-pool-cwmxf"},
            },
        });
        assert_eq!(
            readiness(&claim),
            Readiness::Ready("claude-pool-cwmxf".into()),
            "a claim execs into the sandbox it bound"
        );

        // As agent-sandbox v1.0.2 refuses a claim's env under a Disallowed
        // (or default) template policy: final, so no use waiting it out.
        let rejected = json!({
            "kind": "SandboxClaim",
            "metadata": {"name": "c"},
            "status": {"conditions": [{
                "type": "Ready",
                "status": "False",
                "reason": "EnvVarsInjectionRejected",
                "message": "environment variable injection rejected: environment variable injection is not allowed by the template policy",
            }]},
        });
        assert_eq!(
            readiness(&rejected),
            Readiness::Rejected("environment variable injection rejected: environment variable injection is not allowed by the template policy".into())
        );
        let pending = json!({
            "kind": "SandboxClaim",
            "metadata": {"name": "c"},
            "status": {"conditions": [{"type": "Ready", "status": "False", "message": "Pod exists with phase: Pending"}]},
        });
        assert_eq!(
            readiness(&pending),
            Readiness::Waiting("Pod exists with phase: Pending".into())
        );
        let fresh = json!({"kind": "Sandbox", "metadata": {"name": "s"}});
        assert_eq!(
            readiness(&fresh),
            Readiness::Waiting("waiting for the controller".into())
        );
    }

    #[test]
    fn rfc3339_formats_utc_including_leap_days() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_760_000_000), "2025-10-09T08:53:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(951_782_399), "2000-02-28T23:59:59Z");
    }
}
