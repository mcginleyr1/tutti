// tutti opencode plugin — installed by `tutti hooks opencode --install`.
// Forwards opencode's session state to the tutti pane it runs in, so tutti
// shows exact working / blocked / done states instead of reading the screen,
// and lets tutti answer a permission request (`approvals = "hold"`) while
// opencode's own dialog waits beside it: whichever answers first wins.
// Outside a tutti pane (no TUTTI_PANE) it does nothing. Each forwarded event
// is opencode's own `{ type, properties }`, piped to `tutti agent-event
// opencode`, which maps it and never fails.

const FORWARDED = new Set([
  "session.status",
  "session.idle",
  "session.error",
  "permission.asked",
  "permission.replied",
  "question.asked",
  "question.replied",
  "question.rejected",
]);

// A subagent runs in a child session: its busy/idle is not the agent's own
// (its permission prompts still need you, so those pass).
const ROOT_ONLY = new Set(["session.status", "session.idle", "session.error"]);

export const TuttiPlugin = async ({ $, client }) => {
  if (!process.env.TUTTI_PANE) return {};
  const children = new Set();
  // One send at a time, in order: a `busy` overtaken by its `idle` would
  // leave the pane working.
  let queue = Promise.resolve();
  const send = (payload) => {
    queue = queue
      .then(() =>
        $`tutti agent-event opencode < ${new Response(JSON.stringify(payload))}`
          .quiet()
          .nothrow(),
      )
      .catch(() => {});
  };
  // A permission request's `agent-event` may wait on tutti for an answer, so
  // it starts in order but runs beside the queue; what it prints is the
  // reply to post. Answered in opencode's dialog first, it is killed — or
  // never started — and tutti lets the request go.
  const holds = new Map();
  const hold = (payload) => {
    const id = payload.properties.id;
    holds.set(id, null);
    queue = queue
      .then(() => {
        if (!holds.has(id)) return;
        const agentEvent = Bun.spawn(["tutti", "agent-event", "opencode"], {
          stdin: new Response(JSON.stringify(payload)),
          stdout: "pipe",
          stderr: "ignore",
        });
        holds.set(id, agentEvent);
        new Response(agentEvent.stdout)
          .text()
          .then((reply) => {
            holds.delete(id);
            if (!reply.trim()) return;
            return client._client.post({
              url: "/permission/{requestID}/reply",
              path: { requestID: id },
              body: JSON.parse(reply),
            });
          })
          .catch(() => {});
      })
      .catch(() => {});
  };
  return {
    event: async ({ event }) => {
      const properties = event.properties ?? {};
      if (event.type === "session.created" && properties.info?.parentID) {
        children.add(properties.info.id);
      }
      if (!FORWARDED.has(event.type)) return;
      if (ROOT_ONLY.has(event.type) && children.has(properties.sessionID)) return;
      if (event.type === "permission.asked") return hold({ type: event.type, properties });
      if (event.type === "permission.replied") {
        holds.get(properties.requestID)?.kill();
        holds.delete(properties.requestID);
      }
      send({ type: event.type, properties });
    },
  };
};
