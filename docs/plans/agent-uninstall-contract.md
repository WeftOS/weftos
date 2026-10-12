# agent.uninstall node action

The dashboard queues an `agent_uninstall` request; the node answers the action
kind `agent.uninstall` (`crates/clawft-weave/src/agent_uninstall.rs`), the
counterpart of `agent.install` (ADR-121 O3). It is `weftos team uninstall`'s own
code (`weftos::init::team::uninstall` / `uninstall_package`).

## Payload

Exactly these fields; anything else is refused without being echoed.

```json
{"request_id": "<UUID>", "project_ulid": "<26-char ULID>", "subject": "agent" | "team", "name": "<[a-z0-9-]{1,64}>"}
```

`subject: team` removes that team (packages no other applied team or standalone
install needs). `subject: agent` removes one standalone package.

## Result

- Success: `status: "succeeded"`, `result: {request_id, subject, name, root, removed: [package ids], files: n}`.
- Failure: `status: "failed"`, `result: {error, kind: "agent.uninstall", refused: bool, request_id, edited?: [paths]}`.
  `refused: true` is a decision (nothing written); `false` is an operational failure.

## Refusals (nothing is written)

- Bad payload, project not registered on this node, checkout not on disk.
- Nothing installed under that name (`not installed` / `not applied here`).
- A package an applied team still lists as active (`uninstall that team instead`).
- Any file edited since install: the request is planned first; if any file would
  stay, the whole request is refused and `edited` lists the project-relative
  paths. An edited file is never deleted.

## Chain

Source `agent.uninstall`; kinds `agent.uninstall` (request_id, project_ulid,
subject, name, removed, files) and `agent.uninstall.refused` (same ids, `reason`,
`edited`).
