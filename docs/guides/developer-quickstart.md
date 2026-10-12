# Developer quickstart: a project running on your machine

From nothing to a running project, two ways: the dashboard's **Add locally**
button, or one CLI command. Both do the same thing, because the dashboard's
`install` action and `weaver project add` share one code path
(`crates/clawft-weave/src/project_add*.rs`): fetch, register, detect how the
project runs, install its router route, remember what was created.

## What you need

- WeftOS on this machine (`weaver`, `weft`, `git-remote-weftos` side by side)
  and the user daemon running: `weaver kernel start --profile user`.
- `process-compose` on `PATH` (to run the project).
- To run it by name over the mesh: this node paired with the project's primary
  (the dashboard's pairing flow; see [Installing a project over the
  mesh](project-install.md)). To run it from git: your own git credentials.
- Optional: the tailnet router (`[router] enabled = true`, then
  `weaver route serve --apply`) so the project is reachable at
  `https://<machine>.<tailnet>.ts.net/<slug>/`; see [tailnet-router.md](tailnet-router.md).

## Via the dashboard

1. Open the project in the dashboard and press **Add locally** on its Info panel.
   Pick this machine and, if you want, a path (default `~/Projects/<slug>`).
2. The dashboard queues an `install` action for this node. The daemon fetches
   from the primary over the mesh (or from the project's git remotes when the
   mesh is not available), registers the checkout, detects how it runs and
   writes the route.
3. The action's result carries the same report the CLI prints (`result.add`):
   what runs how, the route, the environment variable names it needs, notes and
   next steps. A refusal (no paired primary, a directory that is not empty)
   says what to do and leaves nothing behind.
4. Continue at "Start it" below.

## Via the CLI

```text
weaver project add weftos://<mesh>/projects/<ULID> --slug shasta
weaver project add git@github.com:org/shasta.git
weaver project add https://github.com/org/shasta.git --path ~/dev/shasta --branch main
```

- **Source.** A `weftos://<mesh>/projects/<ULID>` name is fetched over the mesh
  from the paired primary (through your user daemon, no SSH, no shared secret).
  A git URL is cloned with your own credentials. A dashboard project id (UUID)
  is refused with the way forward, because only the dashboard knows that
  project's repositories: use **Add locally**, or the weftos:// name from the
  project's Info panel. A bare ULID is refused too (it does not say which mesh).
- **Slug.** The handle for everything below. A git URL defaults to the
  repository's name; a mesh name has no project name, so pass `--slug` (or a
  `--path`, whose last segment is used).
- **Registration.** A mesh project is adopted as a *workspace*
  (`weft project init --adopt <ULID>`: no key, chain or kernel here; the primary
  stays where it is). A git URL is registered as a project of its own, unless
  you pass `--project <ULID>` to adopt it as a working copy of an existing one.
- **Detection.** `add` looks for `process-compose.yaml`, `compose/ports.yaml`
  and `package.json` scripts and *says what it found*. It never starts
  anything by guesswork: a project with only `package.json` scripts is told to
  be started by hand.
- **Route.** If the project's own `compose/ports.yaml` declares `routes:`, those
  are used and nothing is written. Otherwise, if exactly one non-process-compose
  port is claimed there, `add` writes `/<slug>` to that port into your route
  overlay (`~/.weftos/routes/<ULID>.yaml`), never into the checkout. No port,
  several ports, a prefix or port another project already routes: no route is
  written and the report says why.
- **Environment.** The names of the variables the project wants are listed from
  `.env.example`-style files and `${VAR}` references in its process-compose
  file, with whether each is set in your shell. Values and `.env` files are
  never copied (the mesh fetch refuses credential-shaped files too).

Example output:

```text
added shasta at /Users/you/Projects/shasta
  fetched by mesh; registered as a workspace (01K6...)
  runs:  process-compose (process-compose.yaml), HTTP :18110
  route: /shasta -> 127.0.0.1:18120 (written to your route overlay; the project declares none)
  env:   OPENROUTER_API_KEY (not set), PORT
next:
  - set these environment variables before starting ...: OPENROUTER_API_KEY
  - weaver project up shasta
  - the app must serve under /shasta (its base path); the router keeps the prefix on the request
  - weaver project status shasta
```

## Start it

```text
weaver project up shasta       # process-compose up -D -t=false -p <its process-compose-http port>
weaver project status shasta   # processes, route, local URL, tailnet URL
weaver project down shasta
```

`up` refuses, by name, `${VAR}`s the compose file needs that are set neither in
your shell nor in the project's own `.env`, and refuses a process-compose port
that something else holds. It needs a `process-compose-http` claim in
`compose/ports.yaml` so `status` and the router can find the process-compose
API. `status` without a slug lists every project added on this machine.

## Undo it

```text
weaver project remove shasta            # stop, drop its route, unregister; keeps the checkout
weaver project remove shasta --purge    # also delete the checkout (refuses uncommitted changes)
```

`remove` undoes exactly what `add` recorded (the receipt in
`~/.weftos/dev-projects/<slug>.json`): the overlay route it wrote and the
manifest it registered. A route the repository declares is part of the checkout
and is not touched. `--purge --force` deletes even with uncommitted changes.

## When it refuses

| You see | Why | Do |
|---|---|---|
| `... already exists and is not empty` | the target path is taken | pick another `--path` |
| mesh fetch: not paired / no primary | this node is not a paired peer of the primary | pair in the dashboard, retry |
| `no process-compose-http claim` | `up` cannot tell which port | add the claim to `compose/ports.yaml` |
| `needs these environment variables` | `${VAR}` unset | export them or put them in the project's `.env` |
| `route: NOT installed: prefix /x is already routed by project y` | route conflict | set a route from the dashboard, or declare `routes:` in `compose/ports.yaml` |
