# COPR package settings

| Field | Value |
| --- | --- |
| Package name | `vigil` |
| Type | Git |
| Clone URL | `https://github.com/washkinazy/vigil.git` |
| Committish | `main` |
| Subdirectory | `packaging` |
| Spec file | `vigil.spec` |
| SRPM build method | `rpkg` |
| Auto-rebuild | enabled |
| Build networking | enabled |

The spec downloads the main-branch source archive and builds with Cargo.lock.
Push the source and spec before starting the first COPR build. Connect the GitHub
push webhook from COPR Settings → Integrations for subsequent builds.

The first package is `vigil-0.1.0-1`. Cargo's workspace version and RPM `Version`
identify the upstream release and must match; CI checks this. RPM `Release`
identifies packaging revisions of that upstream version. Reset it to `1` when
`Version` changes. Tags and automated increment workflows come after the initial
build. Until then, the spec builds the `main` archive.

The RPM installs the two binaries under `/usr/bin`, a vendor systemd user unit,
and the example config under `/usr/share/doc/vigil`. It does not start or restart
Vigil. Existing `~/.local/bin` binaries and a user-level `vigil.service` take
precedence over the packaged files; switching an existing installation is a
separate migration.
