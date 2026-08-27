//! Provider-scoped disk cache under
//! `~/.cache/rootle/providers/rootle-bitbucket/` — the layout the
//! protocol doc recommends (rootle never touches it).
//!
//! Trees are immutable per commit (the whole adapter pins to commit
//! hashes — Bitbucket exposes no git object ids), so tree listings
//! cache by commit and never invalidate. Blobs pass through rootle's
//! own sha-keyed cache; caching them here too saves the API call on
//! repeat visits. Every path component is percent-encoded — values
//! come from API responses and are not trusted to be well-formed.

use crate::api;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    pub is_dir: bool,
    /// Files: "<commit>:<path>" (commit-pinned content id). Dirs: the
    /// commit (they have no content of their own).
    pub sha: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
    pub truncated: bool,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoMeta {
    pub full_name: String,
    pub branch: String,
}

pub struct Cache {
    root: Option<PathBuf>,
}

/// Percent-encode anything outside [A-Za-z0-9_-]: separators, `..`,
/// and NUL can never become path structure.
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Cache {
    pub fn new(root: Option<PathBuf>) -> Self {
        Cache { root }
    }

    pub fn root_as_str(&self) -> Option<String> {
        self.root.as_ref().map(|p| p.to_string_lossy().into_owned())
    }

    fn base(&self) -> Option<PathBuf> {
        let root = self.root.clone().or_else(|| {
            dirs::cache_dir().map(|d| d.join("rootle").join("providers").join("rootle-bitbucket"))
        })?;
        std::fs::create_dir_all(&root).ok()?;
        Some(root)
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, path: PathBuf) -> Option<T> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write_json(&self, path: PathBuf, value: &impl Serialize) -> Option<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(value).ok()?).ok()?;
        std::fs::rename(&tmp, &path).ok()
    }

    /// Repo metadata (branch) by full name — lazily revalidated: a
    /// 404 on use invalidates (moved or renamed repo).
    pub fn repo_meta(&self, full_name: &str) -> Option<RepoMeta> {
        self.read_json(
            self.base()?
                .join("repos")
                .join(format!("{}.json", encode_component(full_name))),
        )
    }

    pub fn store_repo_meta(&self, meta: &RepoMeta) {
        if let Some(base) = self.base() {
            self.write_json(
                base.join("repos")
                    .join(format!("{}.json", encode_component(&meta.full_name))),
                meta,
            );
        }
    }

    pub fn drop_repo_meta(&self, full_name: &str) {
        if let Some(base) = self.base() {
            let _ = std::fs::remove_file(
                base.join("repos")
                    .join(format!("{}.json", encode_component(full_name))),
            );
        }
    }

    /// Tree listing by repo + commit (immutable — never invalidated).
    pub fn tree(&self, full_name: &str, commit: &str) -> Option<Tree> {
        self.read_json(
            self.base()?
                .join("trees")
                .join(encode_component(full_name))
                .join(format!("{}.json", encode_component(commit))),
        )
    }

    pub fn store_tree(&self, full_name: &str, commit: &str, tree: &Tree) {
        if let Some(base) = self.base() {
            self.write_json(
                base.join("trees")
                    .join(encode_component(full_name))
                    .join(format!("{}.json", encode_component(commit))),
                tree,
            );
        }
    }

    /// Blob bytes by content id (immutable). The 1 MiB preview cap is
    /// enforced API-side; this pass-through saves repeat calls.
    pub fn blob(&self, full_name: &str, sha: &str) -> Option<Vec<u8>> {
        let (commit, path) = sha.split_once(':')?;
        std::fs::read(
            self.base()?
                .join("blobs")
                .join(encode_component(full_name))
                .join(encode_component(commit))
                .join(encode_component(path)),
        )
        .ok()
    }

    pub fn store_blob(&self, full_name: &str, sha: &str, bytes: &[u8]) {
        if bytes.len() > api::BLOB_CAP {
            return;
        }
        let Some((commit, path)) = sha.split_once(':') else {
            return;
        };
        if let Some(base) = self.base() {
            let target = base
                .join("blobs")
                .join(encode_component(full_name))
                .join(encode_component(commit))
                .join(encode_component(path));
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp = target.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_encoding_is_structure_safe() {
        assert_eq!(encode_component("main"), "main");
        assert_eq!(encode_component(".."), "%2E%2E");
        assert_eq!(encode_component("a/b"), "a%2Fb");
        assert_eq!(encode_component("feat/x"), "feat%2Fx");
    }
}
