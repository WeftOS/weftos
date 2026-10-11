# Dashboard OS redesign (2026-10-11)

Owner direction, 2026-10-11, for the WeftOS dashboard (weave-logic-ai/weftos-dashboard).
Built in parallel lanes on top of PR #22 (`feat/installations-ux`), integrated into one
branch `feat/os-redesign` and one PR.

## Requirements

### Navigation and URLs
- Every view has a URL (deep links, back/forward, reload keep the place): `/` overview,
  `/projects`, `/projects/<id>` with tabs `info|goals|board`, `/companies`,
  `/companies/<id>`, `/mesh`, `/servers`, `/servers/<host>`, `/ai-org`, `/admin`,
  `/subscriptions`, `/board`, `/goals`.
- Sidebar: remove the long per-project list. Entries: Overview, Projects (with a project
  selector; when a project is selected it shows a submenu Info, Goals, Board), Board,
  Goals, Companies, Mesh Network, Servers, Subscriptions, AI org; **Admin** at the bottom.
- Bug: clicking **AI org** throws the user out of the OS. Fix.

### Overview ("WeaveLogic")
- Title is the control plane's company name (here "WeaveLogic"); for any company with a
  full control plane it is always their company name.
- Minimal for now: a few stats, a **notifications** surface (failed node actions,
  pending pair requests, stale or offline nodes, recent project events, tickets needing
  attention), and a **Key projects** list replacing the long project list.
- Key projects are per control plane (workspace). None are marked yet, so the list shows
  WeftOS, which is always key. Add the ability to mark/unmark a key project (on the project
  page). Rows have no controls beyond: open project details, go to its board, show its
  details, go to its company's board.
- Remove the access link from the bottom of the overview (moves to Admin).

### Projects (first-class)
- Projects have their own page and editor and do not require a company.
- Project page: tabs **Info**, **Goals**, **Board** on one page.
- Info covers everything known: blurb (what it is, what it does), company link if any,
  timeline (started, planned end, milestones — high-level project management), production
  and other URLs, links and docs, where it is installed and served (routes), recent
  activity and who is working on it, history.
- Info is where a developer **adds the project locally**: install on my node, set up its
  route, get it running. Keep to details plus a few functions for now.

### Companies
- Clicking a company name opens company details (it does nothing today).
- Company details: everything in Edit company, its projects with status, last work and
  who is working on them, repositories and their status, history, deals (the Deal column
  is empty; surface deals wherever they live, or say none are recorded).

### Mesh Network (was Installations)
- Rename to **Mesh Network**; focus on the network: every node running WeftOS, its status,
  version, last heartbeat, mesh identity and address, pairings, routes (read-only — the
  owner will not usually change routes here), recent actions.
- Do not manage package installs here (installing a project belongs on the project's Info
  tab). Board credentials and the node token / new node controls move to Admin.

### Servers
- Today they show little and allow nothing; `photo-gallery/shasta` appears as its own server
  with "No host facts synced" and Unknown fields. A project daemon on a machine is not a
  server; a local development copy or an agents' VM is not a server either.
- Fix the host/node model so machines are servers, and project daemons, dev copies and VMs
  are shown as what they are (under their machine). Make the server page useful: status,
  facts, nodes and project daemons on it, served routes, links.

### Admin (bottom of the sidebar)
- Moves here: the access link, board credentials (harness tokens), the node token / add node
  controls, and the "connect project" form from Subscriptions.
- Deep admin functions that cross boundaries (prototype / test functions), plus any settings
  worth exposing (workspace name, domain membership, reporter limits, etc.).

### Subscriptions and the board
- Don't change Subscriptions now beyond moving the connect form to Admin.
- Research: ruflo's federation system as the way to move tickets between control planes
  (an interface on each board that pulls work in and out). Plan only.

## Lanes

| Lane | Owns | Migration version |
|---|---|---|
| shell | `app/` routes, sidebar, URL routing, OS context, AI org fix, Admin page and the moved items | none expected |
| overview | `components/overview/*`, key projects | 202610120001 |
| projects | `components/projects/*` (page, tabs, Info, editor, add locally) | 202610120002 |
| companies | `components/companies/*` (detail page) | 202610120003 |
| mesh | `components/mesh/*`, `components/servers/*`, host/node model | 202610120004 |
| federation | research doc only | none |

Component lanes build prop-driven components and do not edit `app/page.tsx` or the
sidebar; the lead wires them into the shell's routes during integration.
