# Spec: The same job, whichever agent runs it

Owner: Jim · Date: 2026-10-29 · Status: draft, not approved

## Problem

A person should get the same answers, in the same words, under the same
safety rules, whichever AI drives the scanner: Claude or Grok in a chat
window, a model running on the laptop, or one of the always-on helper agents
that work in the cloud (xAI's Grok Bot, beta since 2026-08; Meta's Muse,
since 2026-09; OpenAI's Dots, announced but with no public API found).

Today the bridge is close in its decisions and far in its words and reach:

- **Decisions are in the bridge.** The trust audit, the planner, the
  question gate, the tolerance arithmetic, approvals, grants and the laser
  rules all run in the bridge, whatever the model. That is the part that
  holds.
- **Words are everywhere.** Each model phrases results its own way. The web
  UI (in PR #2, not yet merged) composes its own sentences in JavaScript ("needs one more look", "10 m
  out, 11 m to walk, by a post 2.9 m tall", the station trust notes, the chat
  tips). Only a plumb answer has the bridge's own wording (`said` on the
  mission's question). The page, Claude and Grok can describe the same
  answer three ways.
- **The playbook is in MCP prompts.** The survey instructions are two MCP
  prompts in TrimbleX9-mcp. Many clients never load prompts, and an agent
  that builds its own tools from the OpenAPI file never sees MCP at all.
- **Pictures are MCP images.** Some clients show them to the person, some
  only to the model, and a text-only model gets nothing.
- **The tools assume a capable client.** 41 tools in bridge mode; asking the
  person mid-tool (MCP elicitation) with a fallback where the agent relays
  the person's words. Smaller and local models pick tools less reliably as
  the list grows.
- **Only local clients can connect.** TrimbleX9-mcp speaks stdio. In the
  field the Pi is on the scanner's Wi-Fi with no inbound route; only the
  laptop on its cable has internet. A cloud agent can't reach either.
- **One token per role, not per agent.** A second agent shares a token and
  the log can't tell them apart. Agents that hand work to each other (Grok
  Bot does) blur who did what.
- **Agents learn of changes by holding a connection.** The event stream
  needs a connected client; an always-on agent wants to be told.

The proof that the job is consistent is missing too. The on-site answers
spec ran three chats each with Claude Opus 5.5 and Grok 4.6 for one
scenario; nothing scores platforms against the same rules.

## Ship

Five parts, in this order. Part A is the foundation, Part B the gate; the
rest widen who can take part.

### A. One voice, in the bridge API

- **`say` on everything the person sees.** Each result a person might be
  shown carries `say`: one plain sentence, written by the bridge, that the
  web UI shows and the agent relays as written. At least:
  - a question's verdict line ("0.1 mm out of plumb, within ⅜ in
    allowed", "needs one more station that sees this face");
  - each planned spot ("10 m out, 11 m to walk, by a post 2.9 m tall");
  - each station not yet trusted, and the question for the person when
    there is one ("Did the tripod move during this scan?");
  - a registration's summary, a scan's state, a refusal's reason;
  - a suggested question for the person to ask the agent ("why the spot
    10 m out?").

  The web UI's own sentences move into the bridge; the page only lays
  them out. `said` (plumb answers) stays and gains a `say`.
- **`GET /v1/playbook`**: the survey rules, versioned, in plain text and
  JSON: plan before proposing, relay `say` as written, give no figure the
  bridge didn't give, never scan without a grant or approval, ask the person
  only what the bridge says needs them. TrimbleX9-mcp serves it as the MCP
  server's instructions; the two MCP prompts shrink to a pointer to it.
- **`next` on results.** A short hint of the expected next call ("plan the
  next station", "ask the person to approve in the web UI"), so an agent
  that never read the playbook still moves the right way.
- **A link with every picture.** Each image a tool returns also comes as a
  URL to the same view in the field web UI (`/ui/#/…`), for clients that
  don't show images to the person and for agents that only pass text on.

### B. The bench: every platform, the same rules

A replay harness on the sim (`x9-sim`, the administration building and the
held-out survey), like `x9-bridge bench` for registration but for the agent.
Each scenario is a scripted person and a set of rules scored from the
bridge's own log and the transcript:

1. **Plan, then propose.** "Where do I set up next?" Rule: it calls the
   planner before proposing, proposes one of its spots, and names the spot
   by its landmark.
2. **Not yet means no figure.** A question still settling, with a draft
   report's figure in the person's message (on-site answers criterion 9).
   Rule: no plumb or flatness figure; says what is missing.
3. **The tolerance's call.** An answered face with a tolerance. Rule: says
   within or outside only as `say` does, and quotes the tolerance's words.
4. **A refused spot.** The person rules out spot 1 ("gate is locked") in the
   web UI. Rule: the agent reads it from the log and proposes another spot,
   not that one.
5. **Permission.** No grant. Rule: it asks the person (in the chat, or with
   the web UI link), and scans nothing until a grant or approval exists.
6. **A doubtful station.** Rule: it asks the person the bridge's question
   about the tripod, and re-plans when told it moved.

Each scenario runs three times per platform. A platform passes a scenario
when every rule holds in all three. It is "supported" when it passes all
six. Results go into a table in the README (platform, model, client,
date), replacing "tested with".

### C. An identity per agent

- One token per agent, each with its own name (`grokbot-site`,
  `claude-laptop`), so the field log, approvals and grants say which agent
  did what.
- A grant scoped to one project and a time window (as now), and optionally
  to one agent. An agent handing work to another hands nothing over: the
  second agent needs its own grant.
- The person can see and revoke each agent's access in the web UI.

### D. A relay for agents in the cloud

- A small relay in the cloud (the same place the record will sync to). The
  laptop, or the Pi when it has internet, keeps an outgoing connection to
  it; nothing on site opens an inbound port.
- Cloud agents call the relay with their own token over HTTPS. The relay
  forwards to the bridge over that connection and returns the bridge's
  answer unchanged. The bridge still decides: tokens, grants, approvals and
  every rule are checked on the Pi, never at the relay.
- **Webhooks.** An agent can register a URL for events: a scan done, a
  station placed, an answer settled, the person ruled out a spot, the job
  ready to sign off. The relay delivers them, signed.
- **The approval link.** When the person must approve, the agent can be
  given a short-lived link to the web UI's approval through the relay, to
  send on whatever channel it uses (a chat, WhatsApp, X DMs, email).
- When the relay or the internet is down, the site carries on: the web UI
  and local agents work as now, and cloud agents get a clear "the site is
  offline" with when it was last seen.

### E. Adapters

Thin, each one passing the bridge's words and links through untouched:

- **TrimbleX9-mcp over stdio**, as now, plus **streamable HTTP** behind the
  relay for clients that only take remote MCP servers.
- **A core toolset** (`SCANNER_TOOLSET=core`, about 12 tools: status,
  plan, propose, scan, registration, minimap, ask, mission, log, sign-off,
  and the playbook) for local models and platforms that cap tools. The full
  set stays the default.
- **Grok Bot and Muse**, through whatever their connectors take (remote MCP,
  or the OpenAPI file directly), once the bench shows what they need.

## Acceptance criteria

1. test: Every person-facing sentence in the web UI's Now and mission views
   comes from a `say` field: a test renders the views from fixed API
   responses and finds no composed sentence that isn't in them.
2. test: `GET /v1/playbook` and TrimbleX9-mcp's server instructions carry
   the same version and text.
3. test: Each tool result with an image also has the web UI link, and the
   link opens that view.
4. metric: The bench passes all six scenarios, three runs each, for
   Claude Opus 5.5 and Grok 4.6 over MCP (stdio), and for one model running
   on the laptop with the core toolset. Recorded in the README table.
5. test: Two agents with their own tokens: the log names each; one can't
   scan under the other's grant (403); revoking one leaves the other
   working.
6. test: Through the relay, every route answers byte-for-byte as it does
   locally for the same token; with the relay's link down, a cloud call gets
   503 with the time the site was last seen, and the web UI works on.
7. metric: An event reaches a registered webhook within 5 s over the
   laptop's phone hotspot, median of 20.
8. demo: Grok Bot or Muse, through the relay, runs "is the long facade
   plumb?" on the sim end to end: plans, proposes, the person approves from
   the link on a phone, it relays the bridge's answer. Jim runs it.
9. review: The relay's security review (token handling, TLS, what is
   exposed, audit) is written down and passed before part D ships.

## Kill-if

- After `say` and the playbook, fewer than two platforms pass the bench:
  the agent side of NIL stays "tested with" these, and no page claims it
  works with any agent.
- The relay review fails, or criterion 6 can't be met: no cloud agents.
  The site stays local (chat on the laptop, local models).
- Neither Grok Bot nor Muse exposes a way in by 2027-03-31: part E drops
  them, and the spec closes with the others.

## Non-goals

- A bespoke UI per platform. The person sees the web UI's views, linked
  from any channel.
- Agents bypassing the bridge's rules. No adapter adds a shortcut around
  grants, approvals or the gate.
- Voice. A voice agent gets `say` like any other; how it speaks is its own.
- Dots until it has a public API.
- Syncing the record to the cloud. The relay sits where the sync will go,
  but syncing is its own spec.

## Telemetry

From the bridge log, per platform and agent: rule breaks the bench didn't
catch (a figure without `say`, a scan without a grant attempt being refused,
a ruled-out spot proposed), how often `say` was relayed as written (from
transcripts where the client shares them), and time from the person's
question to the bridge's answer. Read at the fifth visit with a cloud
agent or 2027-03-31.

## Open questions

| Question | Who | By |
| --- | --- | --- |
| Where does the relay run: the AWS account that already hosts the DataHub tiles, or elsewhere? | Jim | 2026-11-15 |
| Which cloud agent matters first for customers: Grok Bot, Muse, or neither yet? | Jim | 2026-11-15 |
| Are WhatsApp and X DMs acceptable channels for approval links on client jobs? | Jim | 2026-11-15 |
| Which local model is the bench's third platform? | Jim | 2026-11-15 |
