# Installing a project from its primary over the mesh (ADR-108 P3b)

A project has one primary installation (normally on a server) and any number of
workspaces on members' machines. This guide covers the mesh path of
"Install on my machine": the member's user daemon fetches the project from the
primary's node over the signed `workload.ctl` wire, with no SSH, no shared
secret and no bytes through the dashboard. The git-remote path (cloning from
the project's own git remotes) is the `git-remote` fetcher and is not covered
here.

One-step flow for developers: [developer-quickstart.md](developer-quickstart.md).

Wire contract: [`docs/plans/adr-108-p2b-p3-contract.md`](../plans/adr-108-p2b-p3-contract.md)
sections 3 and 5. Code: `crates/clawft-weave/src/project_fetch_*.rs` and
`crates/clawft-weave/src/bin/git-remote-weftos.rs`.

## What has to be true

On the **primary's node** (the one whose daemon registers the project with a
role other than `workspace`):

1. `workload-host.json` exists, so the node serves `workload-host`. The member
   is **not** added to `controllers`.
2. `workload-peers.json` lists the member's node **with its key** at tier
   `paired` (what pairing writes) or `pinned`. An entry without a key trusts an
   address, not a node, and never earns fetch access.
3. `project-fetch.json` (same directory, mode 0600) has a grant naming the
   member's node id and the project's ULID:

   ```json
   {"version": 1,
    "grants": [{"peer_node": "<member mesh node id>", "projects": ["<ULID>"],
                "granted_at": "<rfc3339>", "source": "dashboard-pair:<action id>"}]}
   ```

On the **member's node**: `workload-peers.json` lists the primary at tier
`pinned` with its key (pairing writes this too), and the daemon has placement
built (it is, once the file is there).

Both files are re-read on every call. Removing the peer entry or the grant
stops the next request; a transfer in flight stops at its next chunk.

## Who may call what

The primary's controller policy admits three kinds of caller:

| Caller | `workload.describe` | `project.fetch` | everything else (`workload.place`, `dashboard.token.rotate`, ...) |
|---|---|---|---|
| a key in `controllers` | yes | yes | yes |
| a peer with a key at `paired`/`pinned` **and** any grant | yes | yes (only its granted projects) | no |
| anyone else | no | no | no |

`describe` is what the member's control plane needs to learn the primary's key
and address; it returns signed node facts and the service advertisement,
nothing about projects. `project.fetch` then checks the grant for the specific
project on every operation.

## How the transfer works

The node-admin channel is one signed request and one signed response (a
request at most 256 KiB, a response at most 4 MiB). A fetch is therefore a
spooled session on the primary:

- `list` — the project's repositories (`dir`, `head`, `branch`), the size of its
  non-git content, whether that is over the D-C threshold, and the archived
  paths.
- `refs` — branches, tags and HEAD of one repository.
- `bundle.open` — a `git bundle` of the wanted refnames minus what is reachable
  from the `have` commits the member sends (ids the primary does not know are
  ignored). Full history on a clone; only new commits on a later pull. "Nothing
  to send" is an explicit answer, not an empty file.
- `tar.open` — the non-git content as a tar.
- `chunk` — pieces of the size the member asked for at open (16 KiB to 2 MiB;
  an install asks for 2 MiB), each with its own SHA-256; the open answer
  carries the whole file's size and SHA-256 and both are checked by the member.
- `close` — releases the spool file (15 minute TTL otherwise; at most 4 open
  transfers per peer, 32 per node).

Chunks do not travel one connection per call. The member opens a window of
signed sessions to the primary (six by default), each a connection kept open
for the whole transfer, and session `k` reads chunks `k, k+6, k+12, ...`; the
file is written strictly in order. Every request on a session is still its own
signed `CtlRequest` with a fresh nonce and expiry, checked by the primary's
controller policy and replay guard, and every answer is signed and bound to
its request; the grant is re-read per chunk, so a revocation stops the window
at its next reads and the transfer fails (nothing is unpacked). A chunk asked
for with `raw: true` comes back as one unsigned frame after the signed answer,
which carries the frame's size and SHA-256 (the member checks it): no base64,
and the signature covers the hash rather than megabytes of payload. The
`git-remote-weftos` helper gets its chunks base64 over the daemon's local JSON
socket; the daemon keeps a small pool of sessions per primary and uses raw
frames on the mesh leg.

Measured over the in-process signed wire (dev profile, 200 MiB of non-git
content): 1603 round trips at 4.0 MiB/s before, 103 round trips at 11.1 MiB/s
after; with 10 ms added per answer, 3.2 MiB/s before and 12.0 MiB/s after. What
remains is per-byte work on both ends (SHA-256 of every chunk and of the whole
file, the tar build and unpack), not the wire.

Repositories are the project root (`.`), non-hidden directories one level below
it that have a `.git`, and sibling directories a workspace manifest lists; at
most 8. Wants must be refnames the repository really has under `refs/heads` or
`refs/tags`; `dir` must be `.` or one plain path segment. Anything else (`..`,
absolute paths, `.weftos`, revision options) is refused before git runs.

Every open and every refusal is chained on the primary (`project.fetch`,
source `workload.host`) and on the member (`project.fetch`, source
`workload.plane`) with the requester, project, operation and outcome; never a
file's content.

## What is never sent

Git content travels as git: whatever is committed is in the bundle, so an
archived path must be kept out of git on the primary (D-C says marked paths
move to the archive area anyway).

Non-git content is the files outside every repository plus, inside each
repository of the project, the paths git ignores (`git ls-files --others
--ignored --exclude-standard`): data, models, build output. Untracked files
that are not ignored are uncommitted source and stay out. From that set the
primary never sends, and reports the counts of:

- `.weftos/` anywhere (identity, key, chain, certificate), the manifest's chain
  dir and runtime dir wherever they point, and `.git/`;
- paths in `<root>/.weftos/archive.toml`, listed in the result as `archived`
  with their reason:

  ```toml
  version = 1
  [[archive]]
  path = "models/"
  reason = "20 GB of weights; pull them from the model store"
  ```

- symbolic links, whatever they point at;
- credential-shaped names: `.env*`, `*.env`, `.netrc`, `.git-credentials`,
  `.npmrc`, `.pypirc`, `.ssh/`, `.gnupg/`, `.aws/`, `id_rsa*`, `id_ed25519*`,
  `id_ecdsa*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`.

The member unpacks regular files and directories only, each path checked to
stay under the target, nothing overwritten; links and escaping entries are
counted as refused.

**Very large unmarked content** (over 256 MiB of non-git content not in the
archive list) is not fetched by an install: the repositories install, and the
result's `warnings` names the size and the biggest top-level entries so the
member can mark them. The `tar.open` operation itself still serves it when asked
for explicitly.

## Names (ADR-114)

Each clone is left with `origin` on the repository's `weftos://` name:

```text
weftos://<mesh>/projects/<ULID>            the project's root repository
weftos://<mesh>/projects/<ULID>/repos/<dir> a sibling repository
```

`<mesh>` is the mesh's id, 64 lowercase hex, and nothing else: a friendly mesh
name such as `weavelogic` is a dashboard display label and never part of a
name that is stored, chained, granted or sent (owner decision, 2026-10-09);
`weftos://weavelogic/...` is refused as not a name. A node without a mesh id
cannot mint or resolve names and says so. A name identifies a thing, not a
location: the node that serves it is
looked up on the member from `mesh-pairings.json`, which the `pair` action
writes (the primary and the projects the approval named). A name whose
authority is not this mesh, whose project has no paired primary, or whose path
is not a repository gets one and the same refusal, `no such name on this mesh`.

The grammar is `weftos://<authority>/<kind>/<id>[/<segment>...][?rev=sha256:<64 hex>[&view=abstract|overview|content]]`
with kinds companies, projects, goals, tickets, installations, members, nodes,
hosts, services, cogs, teams, agents, memory, sensors (only project
repositories resolve today; nodes are `nodes/<node id>[/services/<name>]`).
Parsing refuses any `%`, non-ASCII, fragments, userinfo, ports, empty,
trailing, `.` and `..` segments, uppercase in structural tokens, unknown or
repeated query keys, and reproduces an accepted name byte for byte
(`crates/clawft-weave/src/weftos_uri.rs`).

## The remote helper

`git fetch`/`git pull` run `git-remote-weftos` (shipped next to `weaver`; it
must be on `PATH`), which speaks git's remote-helper protocol with the `fetch`
capability only: `list` returns the primary's refs and HEAD, `fetch` sends the
tips of every local ref as `have`, receives a bundle of what is missing and
`git bundle unbundle`s it. Push is refused: pushes go to the project's normal
git remote. The helper only parses the name; it talks to the local user daemon
over its RPC socket (`project.fetch` with `{uri, body}`, Admin, which the local
socket owner holds) and the daemon checks the mesh, resolves the primary and
forwards; no network code in the helper.

Install layout: the `.` repository goes to the target path, a sibling `dir` to
`<parent of target>/<dir>`; with no `.` repository every `dir` goes under
`<target>/<dir>`. Non-git content unpacks under the target path. The result is
the contract's `{fetcher: "mesh", root, repos, bytes, archived, warnings}`.

## Checking it

- `weaver` on the member: the install result shows `"fetcher": "mesh"`, each
  repository's short head and its `weftos://` remote.
- On the primary, the chain carries `project.fetch` records with `op`
  `bundle.open`/`tar.open` and `ok: true`; refusals carry `ok: false` and the
  reason (`no fetch grant`, `not a pinned or paired peer`, `does not hold`).
- Negative checks the tests pin down: an unlisted peer and a `discovered` or
  key-less entry are refused at `describe`; a grant for another project is
  refused per project; removing the grant stops the next call and a transfer in
  flight; `dir` and refnames outside the project are refused; archived paths,
  symlinks out of the project and `.env` never arrive.

## Known limits

- Ignored content is collected for the root repository and sub-repositories
  inside the root, not for sibling repositories listed in a manifest.
- A multi-gigabyte tree still costs a SHA-256 pass per chunk on both ends plus
  the whole-file hash; mark what does not belong in a working copy or move it
  to the archive area.
- `git-remote-weftos` has been exercised through its library path (the same
  bundle code the install uses); a clone through a live daemon end to end is the
  first thing to do on real nodes.
