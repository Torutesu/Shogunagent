# Releasing the SDKs and the CLI (E40)

Three packages are published from this repository. **Nothing has been published yet**: the
workflows below are prepared, and the first real publish of each package happens only after the
owner has confirmed it.

| Package | Registry | Source | Workflow |
|---|---|---|---|
| `@shogunagent/sdk` | npm | `packages/sdk-ts` | `.github/workflows/publish-npm.yml` (`package: sdk`) |
| `shogunagent` (the `npx shogunagent` CLI) | npm | `packages/cli` | `.github/workflows/publish-npm.yml` (`package: cli`) |
| `shogunagent` | PyPI | `packages/sdk-py` | `.github/workflows/publish-pypi.yml` |

The Rust binaries and the container image have their own tag-driven workflow (`release.yml`); this
document describes their version boundary below. Native container
builds and the proposed image-only recovery procedure are documented in
[container release recovery](container-release-recovery.md).

## Guard rails

- **Manual only.** Both publish workflows trigger on `workflow_dispatch` and nothing else, and refuse
  to run from any ref but `main`. `scripts/check-workflow-yaml.py` (CI, `release-dry-run` job) fails
  if a `publish-*` workflow gains another trigger.
- **Dry run by default.** `dry_run` defaults to `true`: the run verifies and packs, and stops.
- **Owner approval.** The job that uploads runs in the GitHub environment `release`. Configure that
  environment with the owner as a required reviewer and `main` as the only deployment branch; the
  upload waits for the approval. `scripts/check-workflow-yaml.py` fails if a job holding
  `id-token: write` is outside `release`, or if `id-token: write` is granted workflow-wide.
- **No stored tokens.** npm uses trusted publishing (OIDC from GitHub Actions, npm CLI >= 11.5.1)
  and publishes with `--provenance`; PyPI uses trusted publishing through
  `pypa/gh-action-pypi-publish`, which also uploads attestations. There is no `NPM_TOKEN`,
  `NODE_AUTH_TOKEN` or PyPI token in the repository or its secrets, and none should be added.
- **Typed version.** Each run asks for the version again; `scripts/check-release-versions.mjs`
  fails unless it equals the manifest version, the version constant the code reports
  (`SDK_VERSION` / `VERSION`), and a `## <version>` section exists in the package's `CHANGELOG.md`.
  The run also fails if that version is already on the registry (an anonymous read).
- **Tests before upload.** The verify job runs typecheck, unit tests, build and, for the SDKs, the
  real-framework integration tests (E36b) before the dry run.

## One-time setup (owner, before the first publish)

1. **npm**: create the `shogunagent` org (scope `@shogunagent`). For `@shogunagent/sdk` and
   `shogunagent`, add a trusted publisher: GitHub Actions, repository `Torutesu/Shogunagent`,
   workflow `publish-npm.yml`, environment `release`. A package that does not exist yet must be
   created by its first publish; npm lets a trusted publisher be configured only on an existing
   package, so the very first publish of each name may need a one-time granular token used from a
   maintainer's machine (`npm publish --provenance` is not available there) — or a placeholder
   `0.0.0` published by hand, then trusted publishing for every release after it. Decide which
   before the first release; do not store the token in GitHub.
2. **PyPI**: add a pending trusted publisher for the project `shogunagent`: owner `Torutesu`,
   repository `Shogunagent`, workflow `publish-pypi.yml`, environment `release`. A pending
   publisher works for a project that does not exist yet.
3. **GitHub**: create the `release` environment (required reviewer: the owner; deployment branches:
   `main` only).

## Cutting a release

1. On a branch, bump the version in all three places of the package (manifest, code constant, a new
   `## <version> — <date>` section in `CHANGELOG.md` holding what was under `## Unreleased`), and
   merge to `main` after review. CI runs `scripts/check-release-versions.mjs` and the dry run on the
   PR.
2. Actions → *publish npm* (or *publish PyPI*) → Run workflow on `main` with the package, the
   version and `dry_run: true`. Read the packed file list in the log.
3. Run it again with `dry_run: false`. The upload job waits for the owner's approval in the
   `release` environment; approve it only after reading the dry run.
4. Tag the commit `sdk-ts-v<version>`, `cli-v<version>` or `sdk-py-v<version>` (not `v*`, which
   starts the binary release).

## Versioning

- [Semantic Versioning](https://semver.org). Pre-releases are `X.Y.Z-alpha.N`, `-beta.N`, `-rc.N`
  (npm: published under the `next` dist-tag by passing `--tag next`; PyPI orders them before the
  release by itself). `check-release-versions.mjs` rejects any other shape.
- **0.x**: a minor bump may break the SDK API; a patch never does. From 1.0.0 a breaking change
  needs a major version.
- The three packages are versioned independently. The TypeScript and Python SDKs aim to ship the
  same feature under the same minor version, but nothing forces it.
- What is *not* versioned by the packages: MCP tool wire names, their input schemas and
  annotations are frozen by the server (CLAUDE.md invariant 11; `docs/api/deprecation-policy.md`).
  An SDK release never renames a tool; it can add support for a new name and deprecate the use of
  an old one.
- The CLI downloads the sidecar binaries from the GitHub release named by `--version` (default: the
  latest release). A CLI release that needs a newer binary says so in its changelog and its
  README; it does not pin a binary version of its own.
- Deprecations: a deprecated SDK function keeps working for at least one minor release (0.x) or one
  major release (1.x+), and says so in its doc comment and in the changelog's `Deprecated`.
- Yanking: never unpublish. A broken npm version is `npm deprecate`d with the reason; a broken PyPI
  release is yanked. Both are followed by a fixed patch release.

## Changelog

- One `CHANGELOG.md` per package, next to its manifest, shipped in the npm tarball and the sdist.
- Newest first. A `## Unreleased` section at the top collects changes as they merge; a release
  renames it to `## <version> — <YYYY-MM-DD>` and opens a new empty `## Unreleased`.
- Sections inside a version, as in Keep a Changelog: `Added`, `Changed`, `Deprecated`, `Removed`,
  `Fixed`, `Security`. Write each entry for the person upgrading: what changed for them and what to
  do, one or two sentences, no internal ticket numbers without context.
- A PR that changes a published package's behavior adds its entry under `## Unreleased` in the same
  PR (Conventional Commits still apply to the commit message).

## Rust binary and container releases

The next binary/container release is **v0.2.0**. The shipping Rust packages `shogun-core`,
`shogun-mcp` and `shogun-cli` carry this version; internal Rust libraries, benchmark packages,
Node CLI and SDKs retain their independent versions. Updating these manifests and Cargo.lock
does not publish a registry package. MCP initialization reports the Rust MCP package version.

Prepare version changes, root CHANGELOG.md and `docs/releases/vX.Y.Z.md` on a focused branch.
The tag-driven workflow includes that notes file in the GitHub release. Merge only after
required review/checks, then tag the verified green main commit; never retarget a published tag.
`release.yml` builds Linux x86_64 and ARM64 archives/checksums and native container images;
its image-index job publishes the version tag and latest. Verify binary assets and container
manifest outcomes separately. npm/PyPI publication and hosted deployments are separate actions.
