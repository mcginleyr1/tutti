// tutti opencode plugin — installed by `tutti hooks opencode --install`.
// Forwards opencode's session state to the tutti pane it runs in, so tutti
// shows exact working / blocked / done states instead of reading the screen.
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

export const TuttiPlugin = async ({ $ }) => {
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
  return {
    event: async ({ event }) => {
      const properties = event.properties ?? {};
      if (event.type === "session.created" && properties.info?.parentID) {
        children.add(properties.info.id);
      }
      if (!FORWARDED.has(event.type)) return;
      if (ROOT_ONLY.has(event.type) && children.has(properties.sessionID)) return;
      send({ type: event.type, properties });
    },
  };
};
