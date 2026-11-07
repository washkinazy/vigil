# Releases and COPR

Merge ordinary PRs using Conventional Commit squash titles:

- `fix: ...`: patch release (`0.1.0` → `0.1.1`).
- `feat: ...`: minor release (`0.1.0` → `0.2.0`).
- `feat!: ...` or a `BREAKING CHANGE:` footer: minor before 1.0, major after 1.0.
- `docs: ...`, `test: ...`, and `chore: ...`: no release on their own.

Release Please maintains one release PR with the next version and changelog.
The release workflow enables squash auto-merge only for the generated, same-repository
release branch with the pending-release label and a version-only diff. GitHub waits
for the required `CI` check and an up-to-date branch before merging. Other PRs
remain manually merged. Merging the release PR creates `vX.Y.Z` and a GitHub release. The same Actions workflow
then submits that tag to `washkinazy/vigil` using `copr-cli buildscm` and `rpkg`.
The RPM source URL uses the version tag too, so builds never fetch a moving main
archive. The existing initial `0.1.0` build is the baseline; this configuration
does not republish or increment it on installation.

## One-time setup

1. In GitHub **Settings → Secrets and variables → Actions**, add:
   - `RELEASE_PLEASE_TOKEN`: a fine-grained GitHub PAT restricted to this repo,
     with **Contents**, **Pull requests**, and **Issues** read/write permissions.
     This lets release PRs run validation; the built-in `GITHUB_TOKEN` suppresses
     workflows triggered by its own changes. Approve the token if required by
     the repository owner/organization.
   - `COPR_CONFIG`: the complete INI configuration from the
     [COPR API configuration](https://copr.fedorainfracloud.org/api/), including
     the `[copr-cli]` section. This secret is already configured in this repo.
     Its account must have build permission on `washkinazy/vigil`.
2. In COPR, leave the project's Fedora targets and **Build networking** enabled.
   Disable **Auto-rebuild** on the `vigil` package. Disable/remove the old COPR
   push webhook in GitHub if one was added. Actions now submits releases directly.
3. Enable repository auto-merge and apply `.github/main-ruleset.json` as a GitHub
   branch ruleset. It requires PRs, resolved review threads, and the `CI` check
   from GitHub Actions; it prevents deletion and force pushes with no bypass actors.
   No second-person approval is required for this single-maintainer repository;
   manually merging ordinary PRs is the approval step. Use squash merges with Conventional Commit titles,
   and require the CI checks before merging PRs. Generated release PRs run version
   and release-config validation; their Fedora matrix is skipped only when the
   diff contains release metadata alone. CI-only and documentation-only PRs also
   skip the Fedora matrix. Rust, dependency, packaging, build/install script, and
   unrecognized changes run the full matrix. Workflow validation and CI-selection
   tests run on every PR, alongside release validation.
   Checks run on PRs and manual dispatch, not again on pushes to `main`.

No per-release changes to COPR's Committish or spec settings are required.
The submission passes all source settings each time:

| Setting | Automatic value |
| --- | --- |
| Clone URL | `https://github.com/washkinazy/vigil.git` |
| Committish | Newly created `vX.Y.Z` tag |
| Subdirectory | `packaging` |
| Spec file | `vigil.spec` |
| Build method | `rpkg` |

The submission job prints the COPR build URL and exits after queuing it;
GitHub submission success does not mean the COPR build has finished. COPR signs
and publishes successful RPMs in the existing repository. If submission fails,
rerun the failed job. For a failed COPR build or a deliberate rebuild, run
**Actions → Build COPR release → Run workflow** on `main` and supply the existing
tag. This builds tags created after the automation was installed. A rerun may
create another COPR build of the same version; it never increments a version.

## Version files

`VERSION` holds the shared product version. Release Please updates it alongside
`Cargo.toml`, the three local packages in `Cargo.lock`, `packaging/vigil.spec`,
and `CHANGELOG.md`. Crates continue to inherit the workspace version. CI checks
that they agree and rejects a COPR tag that differs from the package version.
The release manifest is Release Please's bookkeeping; release PRs update it
alongside the version files.

The simple strategy with targeted TOML updates preserves the virtual workspace's
inherited versions; the Rust strategy does not support that layout reliably.
Keep RPM `Release` at `1` for these upstream releases. Packaging fixes that need
a new published package use `fix(packaging): ...` and receive a patch version.
Dependency updates never rewrite unrelated locked versions during a release bump.

No service is enabled, started, or restarted by the release workflow. Switching
an existing local installation to the RPM remains a separate migration.

References: [Release Please](https://github.com/googleapis/release-please-action),
[COPR SCM builds](https://docs.copr.fedorainfracloud.org/user_documentation.html#scm).
