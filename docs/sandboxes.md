# Kubernetes sandboxes

How to run an agent in a Kubernetes pod instead of on your machine while its
pane, state badge and waiting-queue entry stay in tutti: sandbox profiles,
what a sandbox pane does from creation to deletion, how code gets in and work
comes back out, and reporting state from inside the pod.

## What a sandbox pane does

An agent can run in a Kubernetes sandbox instead of on your machine. You get
tutti's view of the agent unchanged: the pane, its state badge, the waiting
queue, and answering from the queue. The agent itself runs in a pod. tutti
drives [agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox)
(`agents.x-k8s.io/v1beta1`) through your own `kubectl`, so your kubeconfig,
contexts, and auth plugins (GKE's included) work as they already do.

Configure one or more `[[sandboxes]]` profiles (see
[Configuration](configuration.md)). Then press `tab` in the run launcher to
target one, or from scripts run `tutti run --sandbox <profile> -- claude` (or
`pane run --sandbox`). The pane runs `tutti sandbox run <profile> --name
<name> -- claude`, which:

1. **creates** the sandbox, named by the daemon for its pane
   (`tutti-p<pane>-<hex>`, passed as `--name`). A `warm_pool` profile creates a
   `SandboxClaim` on that `SandboxWarmPool`, which is fast because a pod is
   already waiting. An `image` profile creates a `Sandbox` whose single
   container, `agent`, runs that image. Either is labelled
   `app.kubernetes.io/managed-by=tutti`.
2. **waits** until it is Ready, echoing the controller's status messages into
   the pane (ctrl-c cancels). A claim whose env its template refuses fails at
   once, saying so.
3. **execs** the agent with `kubectl exec -it` (forwarding `TERM` and
   `COLORTERM`). The pane's pty stays local, so the screen classifier, OSC
   9;4/7501 reports, bells, and guarded answers all work as they do for a local
   agent.
4. **deletes** the sandbox however the pane ends: the agent exits, the pane is
   killed, the daemon stops, or you press ctrl-c. A watchdog in its own
   process group waits on a pipe that only the pane process holds, and runs the
   delete once that pipe closes. As a backstop, the sandbox's `shutdownTime` is
   kept a full `ttl_minutes` ahead (renewed every third of it), with
   `shutdownPolicy: Delete`. If tutti dies hard, the cluster reaps the sandbox
   within the TTL.

## Code in, work out

**The sandbox owns the code.** tutti does not copy your checkout in or out. The
image or template clones the repo, and the agent pushes its work. The sandbox
is told where: `TUTTI_GIT_REMOTE`, the workspace's git remote (`origin`, else
the first `jj git remote`), and `TUTTI_BRANCH`, `tutti/<sandbox name>`, the
branch to push to. An `image` sandbox's container always gets both; its
entrypoint has to keep running for the exec, for example `git clone
"$TUTTI_GIT_REMOTE" /work && exec sleep infinity`. A warm-pool claim carries
them only with `inject_env = true`, and only if the pool's `SandboxTemplate`
sets `envVarsInjectionPolicy: Allowed`: the default rejects the claim, and
tutti fails the pane at once saying so. Injecting env costs the pool's head
start, since a running pod cannot take env and the claim gets a fresh one.
Without `inject_env`, the template has to know the repo itself.

**Pulling the work back.** `tutti sandbox pull <pane> [--name <workspace>]` —
or `p` on the exited sandbox pane, whose title reads `r resume · p pull · x
close` — fetches the sandbox's branch from the project's remote, tracks it, and
checks it out as a workspace nested under the project, named after the sandbox
unless you pass `--name`. Its diff, the changes panel, and merge then work as
for any workspace. It refuses a pane that is not a sandbox's, a repo with no
remote, and a branch the sandbox has not pushed. The TUI's `p` asks for no
name, so pulling the same sandbox twice takes the CLI's `--name`.

## Hooks from the pod

**Hooks from the pod.** `tutti agent-event` can't reach the daemon from a pod,
but the pane's pty can. `tutti hooks claude --tty` prints a Claude Code hooks
config whose commands write OSC 7501 straight to `/dev/tty` with plain
`printf`, so the image needs no tutti binary: a prompt or tool use reports
**working**, a permission or question Notification **blocked**, Stop **done**,
and SessionEnd clears. Each command is silent and never fails. To bake it in,
run `tutti hooks claude --tty --raw > hooks.json` and merge it into the agent
user's `~/.claude/settings.json` (top level) as the image is built, or commit
it as `.claude/settings.json` in the repo the sandbox clones. The pane's state
then comes from source `program`. Its Notification matcher names the waiting
types, so a type it does not know yet will not block, unlike through
`agent-event`.

## How a sandbox pane differs

The pane title and sidebar mark where the agent runs (`claude on gke`). Since
the local process tree only ever shows `tutti` and `kubectl`, the agent kind
comes from the command, not from detection. Known limits: without the `--tty`
hooks, state comes from the screen. Turns and checkpoints stay local and say
nothing about the sandbox's work, so sandbox panes record none; pull the work
back for its diff. Resume rows are hidden in a sandbox launcher because those
conversations live on the daemon's machine. An exited sandbox pane's `r` reopens the
launcher on the same sandbox.
