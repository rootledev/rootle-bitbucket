# rootle-bitbucket

[![ci](https://github.com/rootledev/rootle-bitbucket/actions/workflows/ci.yml/badge.svg)](https://github.com/rootledev/rootle-bitbucket/actions/workflows/ci.yml)
[![conformance](https://github.com/rootledev/rootle-bitbucket/actions/workflows/conformance.yml/badge.svg)](https://github.com/rootledev/rootle-bitbucket/actions/workflows/conformance.yml)
[![audit](https://github.com/rootledev/rootle-bitbucket/actions/workflows/audit.yml/badge.svg)](https://github.com/rootledev/rootle-bitbucket/actions/workflows/audit.yml)
[![crates.io](https://img.shields.io/crates/v/rootle-bitbucket.svg)](https://crates.io/crates/rootle-bitbucket)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Bitbucket Cloud provider for [rootle](https://rootle.dev) — browse,
preview, search, and clone Bitbucket repos without cloning anything
first.

Speaks the [rootle stdio provider protocol](https://github.com/rootledev/rootle/blob/main/doc/provider-protocol.md)
(v1.5) over Bitbucket Cloud's REST 2.0 API, and is gated by
[rootledev/forge-conformance](https://github.com/rootledev/forge-conformance)
(v1.5.1, 47 cases) in CI. Shares no code with rootle; the wire
contract is the entire interface.

## Install

The provider manager does it all — download, sha256 verification, and
the config swap:

```
rootle provider install bitbucket   # rootledev releases, checksum-verified
rootle provider use bitbucket       # point [provider] at it
```

## What it supports

Bitbucket Cloud has **no code-search index and no blame API** — the
handshake says so (`code_search: false, file_search: true` for the
v1.3 split; `blame: false` for v1.5, the honest answer — dispatch has
no `repo/blame` arm, so the call fails as an unknown method instead of
a stub that fake-succeeds):

- **Browse**: workspaces → repos → full recursive trees (walked and
  cached per commit — Bitbucket lists one directory per call).
- **Preview**: syntax-highlighted blobs, pinned to commit hashes
  (Bitbucket exposes no git object ids, so `<commit>:<path>` is the
  content id).
- **Revisions** (v1.5): `repo/refs` (branches + tags, one default
  marker), `repo/tree` at a branch/tag/sha ref (resolved fresh each
  call; unknown ref → `not_found`; the reply names the ref served),
  `repo/log` (newest-first, path filter, `limit`-bounded), and
  `repo/blob_at` (a path's bytes at a ref, with the same content id
  the tree carries).
- **File find**: `path:`/`extension:` search over the cached tree,
  served as path-only hits.
- **Grep**: bare terms grep the fetched blobs through the same cache
  the preview reads — binary-skipping, line-anchored, capped (no
  index, just the tree; `code_search: false` stays — this is bounded
  best-effort, not a global index). `search/code` streams per-repo
  `$/partial` batches when rootle asks (`partial: true`).
- **Clone**: the wizard uses the repo's https clone URL.
- **Yank**: browser URLs with `#lines-N` fragments.

## Credentials

Read lazily on first API call — never at spawn, per the protocol's
restart obligations:

```
BITBUCKET_USERNAME=you          # with an app password:
BITBUCKET_TOKEN=your-app-password
```

or a bearer token alone:

```
BITBUCKET_TOKEN=your-api-token
```

## Live testing

Fork-scale smoke against the real API (dispatch-only):

```
gh secret set BITBUCKET_LIVE_USERNAME --repo rootledev/rootle-bitbucket
gh secret set BITBUCKET_LIVE_TOKEN    --repo rootledev/rootle-bitbucket
gh workflow run live.yml --repo rootledev/rootle-bitbucket
```

Same app-password scopes as normal use (Account — Read,
Repositories — Read). Set `BITBUCKET_LIVE_WORKSPACE` as a repo
*variable* (not a secret) to exercise a specific workspace.

## Conformance

Correctness is gated by
[rootledev/forge-conformance](https://github.com/rootledev/forge-conformance)
(v1.5.1, 47 cases) on every push. The harness is
`examples/forge_conformance`: the real adapter serving the canonical
fixture through an in-process Bitbucket mock — plain directories for
the frozen repos, git itself for the suite's revision repo. Locally:

```
cargo build --locked --example forge_conformance
git clone https://github.com/rootledev/forge-conformance /tmp/fc
cd /tmp/fc && PROVIDER=../rootle-bitbucket/target/debug/examples/forge_conformance python3 run
```

## Development

```
docker compose run --build --rm test   # fmt + clippy + wiremock suite
```

## Advanced: manual setup

`cargo install rootle-bitbucket`, or a prebuilt static tarball from
the [releases](https://github.com/rootledev/rootle-bitbucket/releases)
(sha256 sidecar included), then wire the binary in by hand:

```toml
[provider]
kind = "stdio"
command = ["rootle-bitbucket"]
# Self-hosted? Point it elsewhere:
# command = ["rootle-bitbucket", "--instance", "https://api.bitbucket.example.com"]
```

MIT.
