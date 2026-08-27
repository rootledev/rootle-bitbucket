# rootle-bitbucket

Bitbucket Cloud provider for [rootle](https://rootle.dev) — browse,
preview, grep-free search, and clone Bitbucket repos without cloning
anything first.

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
- **File find**: `path:`-scoped search over the cached tree, served as
  path-only hits. Content grep answers with an honest error.
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

## Development

```
docker compose run --build --rm test   # fmt + clippy + wiremock suite
```

MIT.
