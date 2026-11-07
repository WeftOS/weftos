# Sandbox survey: bwrap, Landlock, user namespaces (2026-11-07)

Card 4dbc483c (Tapestry decision Q9). Read-only probes plus throwaway smoke tests in a temp dir; nothing installed.

Status: **partial**. Only the CI host (aepod-xpc, WSL2) was surveyed. PG, the SC and the ARM VM were not reachable from this session (`ssh pg` does not resolve); they still need the same probes.

## CI host (WSL2 on aepod-xpc)

| Item | Result |
|---|---|
| Kernel | 6.18.54.1-microsoft-standard-WSL2 |
| Landlock ABI | 7 (`landlock_create_ruleset` version query) |
| Unprivileged user namespaces | work (`unshare -Ur`); `max_user_namespaces` 386194; no AppArmor restriction sysctl present |
| bubblewrap | 0.11.1 (`/usr/bin/bwrap`) |
| `bwrap --overlay-src/--overlay` | works; writes land in the upper dir |
| Kernel overlayfs in a user namespace | works (`unshare -Urm`, `mount -t overlay`) |

Smoke tests (all passed):

- `bwrap --unshare-all --ro-bind / / --bind <dir> /mnt`: write to `/mnt` ok, write to `/etc` refused (read-only filesystem), outbound connect fails (network unreachable).
- `bwrap --overlay`: write inside the overlay succeeds and appears only in the upper dir.
- Landlock (ABI 7 ruleset, write rights granted to one directory): write inside the tree allowed, write to a sibling directory and to `/tmp` denied with EACCES.

Not covered: Landlock applied inside the bwrap child (spike plan note), read-restriction rules, and the Tapestry C2/C6 gate scripts themselves.

## Still to survey

| Host | Needed |
|---|---|
| PG | same table; host survey says a bare `unshare` is blocked there |
| SC (Spot VM) | kernel and Landlock ABI (also needed for ADR-131 D6) |
| ARM VM | same table |
