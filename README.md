# rootle-bitbucket

[![ci](https://github.com/rootledev/rootle-bitbucket/actions/workflows/ci.yml/badge.svg)](https://github.com/rootledev/rootle-bitbucket/actions/workflows/ci.yml)
[![conformance](https://github.com/rootledev/rootle-bitbucket/actions/workflows/conformance.yml/badge.svg)](https://github.com/rootledev/rootle-bitbucket/actions/workflows/conformance.yml)
[![audit](https://github.com/rootledev/rootle-bitbucket/actions/workflows/audit.yml/badge.svg)](https://github.com/rootledev/rootle-bitbucket/actions/workflows/audit.yml)
[![crates.io](https://img.shields.io/crates/v/rootle-bitbucket.svg)](https://crates.io/crates.io/rootle-bitbucket)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Bitbucket Cloud provider for [rootle](https://rootle.dev) — browse,
preview, search, and clone Bitbucket repos without cloning anything
first.

Speaks the [rootle stdio provider protocol](https://github.com/rootledev/rootle/blob/main/doc/provider-protocol.md)
(v1.3) over Bitbucket Cloud's REST 2.0 API. Shares no code with
rootle; the wire contract is the entire interface.

## What it supports

Bitbucket Cloud has **no code-search API** — the handshake declares
`code_search: false, file_search: true` (the protocol's v1.3 split):

- **Browse**: workspaces → repos → full recursive trees (walked and
  cached per commit — Bitbucket lists one directory per call).
- **Preview**: syntax-highlighted blobs, pinned to commit hashes
  (Bitbucket exposes no git object ids, so `<commit>:<path>` is the
  content id).
- **File find**: `path:`/`extension:` search over the cached tree,
  served as path-only hits.
- **Grep**: bare terms grep the fetched blobs through the same cache
  the preview reads — binary-skipping, line-anchored, capped (no
  index, just the tree; `code_search: false` stays — this is bounded
  best-effort, not a global index). `search/code` streams per-repo
  `$/partial` batches when rootle asks (`partial: true`, v1.3).
- **Clone**: the wizard uses the repo's https clone URL.
- **Yank**: browser URLs with `#lines-N` fragments.

## Install

```
cargo install rootle-bitbucket
```

or via rootle's provider manager (once published as a release):
`rootle provider install bitbucket`.

## Configure

`~/.config/rootle/config.toml`:

```toml
[provider]
kind = "stdio"
command = ["rootle-bitbucket"]
# Self-hosted? Point it elsewhere:
# command = ["rootle-bitbucket", "--instance", "https://api.bitbucket.example.com"]
```

Credentials (read lazily on first API call — never at spawn, per the
protocol's restart obligations):

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

[forge-conformance](https://github.com/rootledev/forge-conformance)
(rootledev/rootle plans/0015) — the canonical 37-case protocol suite —
runs against this adapter on every push. The harness is
`examples/forge_conformance`: the real adapter serving the canonical
fixture through an in-process Bitbucket mock. Locally:

```
cargo build --locked --example forge_conformance
git clone https://github.com/rootledev/forge-conformance /tmp/fc
cd /tmp/fc && PROVIDER=../rootle-bitbucket/target/debug/examples/forge_conformance python3 run
```

## Development

```
docker compose run --build --rm test   # fmt + clippy + wiremock suite
```


MIT.
