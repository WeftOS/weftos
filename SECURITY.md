# Security

Report a vulnerability through a GitHub private advisory:

<https://github.com/WeftOS/weftos/security/advisories/new>

Do not open a public issue for a security report.

Releases are signed. `weaver update` refuses a release whose signed manifest
does not verify; see [docs/deployment/install.md](docs/deployment/install.md).
Do not commit keys, `.env` files or tokens to this repository.
