# Spec: The job under someone else's AI control plane

Owner: Jim · Date: 2026-11-01 · Status: draft, not approved

The companion to [`any-agent.md`](any-agent.md). That spec makes the job the
same whichever agent drives the scanner: the bridge's own words (`say`), the
playbook, the bench, an identity per agent, the relay and the adapters. This
one covers what that spec leaves out once the agent's model is called through
a control plane that is not ours: what leaves the site, who decided under
what, what a mission costs, and what the plane's administrator needs before
the agent may connect at all. It adds no words, no bench rules and no
transport; it points at `any-agent.md` for those.

An AI control plane, here, is the gateway an organization puts between its
people and the model providers: one endpoint that holds the provider keys,
routes each call to an allowed model, meters spend per person or team,
enforces policy on what may be sent, and logs prompts and completions with a
retention of its own. A university, a utility and a county each run one or
will. When the scanning agent runs there, every tool result the bridge
returns is a prompt input to that plane.

## Problem

No code of ours calls a model. The chat client on the laptop (Claude Code,
Grok) calls it, on the operator's own account, and the bridge and
TrimbleX9-mcp are tool servers. That is the right shape and it stays. But a
customer's agent will run under the customer's plane, and four things are
then unspecified:

- **What leaves the site.** A tool result today can carry the project name
  (typed by the person, often the client or the street), the mission
  sentence, every `say`, the figures, the field log with people's notes, and
  pictures: station views, heat maps, previews and the minimap. The minimap
  of a house is its floor plan. `docs/principles.md` says scans of homes are
  private and the fixture rules keep them out of git, but nothing says which
  of these may leave the site to a model whose plane logs them, and the
  person on site is never told that they do.
- **Who decided, under what.** A field log entry has `author`, the token's
  name; a mission has `created_by`; a proposal has `proposed_by`. None
  records the client, the model, the plane, or the playbook version the
  agent followed. `any-agent.md` part C gives each agent its own token and
  part B records platform, model, client and date in the README table. The
  record of one mission still cannot say which model proposed station 7, so
  a customer reviewing a sign-off cannot either.
- **The bill.** A plane meters tokens per person and team and stops a call
  that breaks a budget. Nobody has measured what a mission costs in tokens;
  the bench scores rules, not spend. A budget that trips at station 9 is an
  unplanned stop in the field.
- **The way in.** A plane's administrator allows tool servers by list and
  wants, before saying yes: what the server is, how it is reached and
  authenticated, every tool and what each returns, what data flows where,
  what is logged, what connects outbound, and whom to call. We have a guide
  for the operator and a README for the engineer. There is no page for the
  administrator, and the answers are spread over both.

## Ship

Four parts. A stands on its own and ships first; B, C and D follow
`any-agent.md` parts A and B, which they lean on.

### A. A disclosure level per project

Each project carries `disclosure`, set when it is created or changed by the
operator in the web UI, and written to the field log when it changes:

| Level | A tool result may carry | It never carries |
| --- | --- | --- |
| `full` | Everything it does today | — |
| `figures` | `say`, verdicts, figures, plans, the mission, the agent's own log entries, and for every picture a link to the same view in the field web UI (`any-agent.md` part A) | Picture bytes |
| `minimal` | As `figures`, with the project named only by its `project_id` | Picture bytes, the project's name, log entries written by people |

The bridge enforces it in the API, per token role: an agent token at
`figures` gets no image bytes from any route, `curl` included; the operator
and viewer tokens in the web UI see everything, since the web UI stays on
site. TrimbleX9-mcp passes results through and adds nothing. The playbook
states the level in one sentence, so the agent knows why a picture is a
link. The mission card in the web UI shows the level in one line to the
person ("Figures and plans from this job go to the agent's model. Pictures
stay here."), with the agent's name from its token.

The default for a new project is an open question below; until it is
decided, the default is `full`, which is what happens today.

### B. Who decided, under what

An agent's token gains a declaration: `client`, `model` and `plane`, as the
deployment states them (TrimbleX9-mcp reads `clientInfo` from the MCP
handshake for `client`, and `SCANNER_MODEL` and `SCANNER_PLANE` from its
environment for the rest). The bridge cannot verify a model from a tool
call, so the record says `declared` beside these, and the README says so.

Every log entry, mission, proposal and sign-off made by an agent carries `agent: {name, client, model, plane, playbook}`, the playbook being
the version of `GET /v1/playbook` the agent was served. The report's
sign-off page prints one line per agent that took part: who it was, what it
declared, which playbook, and its first and last entry. Two agents on one
mission are two lines.

### C. The bill, and finishing without the agent

The bench (`any-agent.md` part B) records, beside pass and fail, the tokens
in and out and the dollars for each scenario run, from the client's usage
report where the client gives one, and the README table carries them. The
survey prompt run end to end on the demonstration scan replay is
measured the same way, three runs, and its figure is written here.

The mission never needs the agent to finish. When the plane refuses (a
budget, a policy, a rate limit) or the agent is gone, the operator completes
the mission from the web UI: asks again, sets up where the plan says, signs
off. Where today's web UI needs the agent for a step, that step gets a
control. The sign-off line says who signed and where the agent's last entry
was.

### D. The administrator's page

`docs/for-the-plane-administrator.md`, one page, for the person who allows
tool servers on a customer's plane:

- What the server is and what it drives, in four sentences.
- How it is reached: stdio on the laptop today; streamable HTTP through the
  relay when `any-agent.md` part D ships. How it is authenticated: one token
  per agent, its role, how it is revoked.
- Every tool, with what it returns at each disclosure level. This table is
  generated from the server's own tool list, so it cannot drift.
- What is logged, and where: the field log on the bridge computer, nothing
  sent to North Idaho Labs.
- What connects outbound from the site: nothing today; the relay's one
  connection when enabled.
- Versions, the OpenAPI file, and whom to contact.

## Acceptance criteria

1. test: At `figures`, no tool result from any route carries image bytes,
   and every result that carried a picture at `full` carries a web UI link
   in its place that opens that view. At `minimal`, no result carries the
   project's name or a log entry authored by a person. At `full`, results
   are byte-for-byte what they are today.
2. test: The level binds the API, not the adapter: a direct request with an
   agent token at `figures` gets no image bytes; the same request with the
   operator token gets them. Changing the level writes a field log entry
   naming who changed it and to what.
3. test: Every log entry, mission, proposal and sign-off by an agent
   carries `agent` with `name`, `client`, `model`, `plane` and `playbook`,
   the declared fields marked declared. Two agents with different
   declarations on one mission are told apart in the record and in the
   report's sign-off page.
4. test: With the agent's connection dropped after a station, the operator
   finishes the mission from the web UI and signs off; the sign-off line
   names the signer and the station of the agent's last entry.
5. metric: The bench table carries tokens and dollars per scenario for every
   platform whose client reports usage, and the survey prompt on the
   demonstration scan replay has its three-run total written here.
6. test: The administrator's page lists every tool the server exposes with
   what it returns at each level; a tool added without a row fails the
   test.
7. review: Someone who runs a plane and is not Jim reads the page and allows
   the server without a question the page should have answered. Each
   question asked becomes a line on the page. Jim finds the reader.
8. demo: One mission on the sim through a gateway that is not the operator's
   own account, at `figures`: a LiteLLM or Portkey instance on the lab
   machine in front of the same model counts. The plane's log holds no
   picture and no project name; the bridge's record names the client, model
   and plane; the plane's own ledger and the client's usage report agree on
   the tokens within 5%. Jim runs it.

## Kill-if

- Two platforms that pass the `any-agent.md` bench at `full` fail it at
  `figures`: the pictures are needed. `figures` and `minimal` go, the level
  becomes a statement of what leaves (part D) rather than a switch, and the
  web UI line stays.
- A declaration proves wrong against the plane's log in the demo or the
  bench more than once: `model` leaves the record, `client` and `plane`
  stay, and the README says declaration was not enough.
- No customer conversation asks for any of B, C or D by 2027-03-31: those
  parts wait. Part A ships anyway, because the privacy principle stands
  without a customer.

## Non-goals

- Hosting a control plane, or selling one. Customers have theirs.
- Calling a model from the bridge or TrimbleX9-mcp. There is no such call
  today and this spec adds none; there is no provider to abstract.
- Verifying the declared model. No client exposes a verifiable model id to a
  tool server.
- Per-provider or per-model prompts. The playbook is one text; a model that
  needs its own is a bench failure, not a branch.
- Redacting people's own words in the field log beyond withholding them at
  `minimal`.
- The relay, streamable HTTP and the core toolset: `any-agent.md` parts D
  and E.
- Syncing the record off site.
- Any public claim that the agent "works with your gateway" before criteria
  7 and 8 pass.

## Telemetry

From the bridge log, per mission: the disclosure level and any change to it,
each agent's declaration, the tokens where the client reported them, whether
an agent was present at sign-off and where its last entry was, and each
refusal the agent reported from its plane (budget, policy, rate) with how
the mission finished after it. Read at the fifth customer mission under a
plane that is not ours, or on 2027-03-31.

## Decided

| Date | Question | Decision | Why |
| --- | --- | --- | --- |
| 2026-11-01 | Where does this live? | Here, beside `any-agent.md`, not in the lab site's specs. | The bridge enforces every part of it; the site has no model call and no scanner. `any-agent.md` owns words, bench, identity, relay and adapters; this spec owns what leaves, provenance, the bill and the administrator's page, and neither restates the other. |
| 2026-11-01 | Does any code of ours call a model? | No. The client calls it, through the person's or the customer's plane. | Portability then costs nothing to keep: there is no provider in our code to abstract, and an adapter layer would be an abstraction over nothing. The spec adds none. |
| 2026-11-01 | Where is disclosure enforced? | In the bridge API, per project and token role. Not in the MCP server, not in the playbook. | An adapter that is not ours cannot be trusted to strip, and a playbook rule binds only a model that read it. A rule in the API binds every client, including a `curl` from a cloud agent through the relay. |
| 2026-11-01 | Is the model verified or declared? | Declared, and the record says so. | No client gives a tool server a verifiable model id. An honest label beats a false certainty; the demo and the bench cross-check declarations against the plane's log where there is one. |

## Open questions

| Question | Who | By |
| --- | --- | --- |
| Default disclosure for a new project on a client job: `full` or `figures`? | Jim | 2026-11-30 |
| Which gateway for the demo: LiteLLM or Portkey on the lab machine, or a customer's? | Jim | 2026-11-30 |
| Does the administrator's page go in this public repo, or to each customer only? | Jim | 2026-11-30 |
| Does the token's declaration (part B) fold into `any-agent.md` part C when that is built, or stay here? | Jim | 2026-11-30 |
