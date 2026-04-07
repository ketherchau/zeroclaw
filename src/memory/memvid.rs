//! Memvid-backed memory — persistent, portable, single-file (`.mv2`).
//!
//! Each `MemvidMemory` instance wraps one `.mv2` file. Facts are stored as
//! text frames and retrieved via BM25 full-text search (Tantivy, sub-5ms).
//!
//! "forget" stores a tombstone frame rather than deleting (append-only model).

use super::traits::{Memory, MemoryCategory, MemoryEntry};
use anyhow::Context;
use async_trait::async_trait;
use chrono::Utc;
use memvid_core::{
    memvid::lifecycle::Memvid,
    types::{PutOptions, SearchRequest},
};
use parking_lot::Mutex;
use std::{path::PathBuf, sync::Arc};
use uuid::Uuid;

pub struct MemvidMemory {
    inner: Arc<Mutex<Memvid>>,
    path: PathBuf,
    namespace: String,
}

impl MemvidMemory {
    /// Open or create a `.mv2` memory file at `path`.
    pub fn open_or_create(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path: PathBuf = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create directory {:?}", parent))?;
        }
        let mem = if path.exists() {
            Memvid::open(&path).with_context(|| format!("open {:?}", path))?
        } else {
            Memvid::create(&path).with_context(|| format!("create {:?}", path))?
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(mem)),
            path,
            namespace: "default".to_string(),
        })
    }

    /// Return the file path for this memory store.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

/// Build a search request with all required fields.
fn search_req(query: &str, top_k: usize) -> SearchRequest {
    SearchRequest {
        query: query.to_string(),
        top_k,
        snippet_chars: 300,
        uri: None,
        scope: None,
        cursor: None,
        as_of_frame: None,
        as_of_ts: None,
        no_sketch: false,
        acl_context: None,
        acl_enforcement_mode: Default::default(),
    }
}

#[async_trait]
impl Memory for MemvidMemory {
    fn name(&self) -> &str {
        "memvid"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let now = Utc::now().to_rfc3339();
        // Encode metadata as a JSON prefix in search_text so it's indexed.
        let search_text = format!(
            "[key={key}] [cat={category}] [ns={}] [ts={now}] {content}",
            self.namespace
        );

        let opts = PutOptions {
            title: Some(key.chars().take(80).collect()),
            uri: Some(format!("mv2://memory/{key}")),
            search_text: Some(search_text),
            dedup: false,
            extract_triplets: false,
            extra_metadata: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("key".to_string(), key.to_string());
                m.insert("category".to_string(), category.to_string());
                m.insert("namespace".to_string(), self.namespace.clone());
                m.insert("created_at".to_string(), now.clone());
                if let Some(sid) = session_id {
                    m.insert("session_id".to_string(), sid.to_string());
                }
                m
            },
            ..PutOptions::default()
        };

        let inner = self.inner.clone();
        let content_bytes = content.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut mem = inner.lock();
            mem.put_bytes_with_options(&content_bytes, opts)?;
            mem.commit()?;
            Ok(())
        })
        .await
        .context("spawn_blocking store")??;

        Ok(())
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let inner = self.inner.clone();
        let query = query.to_string();
        let session_filter = session_id.map(|s| s.to_string());
        let namespace = self.namespace.clone();

        let hits = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<_>> {
            let mut mem = inner.lock();
            let resp = mem.search(search_req(&query, limit * 2))?;
            Ok(resp.hits)
        })
        .await
        .context("spawn_blocking recall")??;

        let entries: Vec<MemoryEntry> = hits
            .into_iter()
            .filter_map(|h| {
                let meta = h.metadata?;
                let key = meta.extra_metadata.get("key")?.clone();
                let cat_str = meta.extra_metadata.get("category").cloned().unwrap_or_default();
                let ns = meta.extra_metadata.get("namespace").cloned().unwrap_or_default();
                let sid = meta.extra_metadata.get("session_id").cloned();
                let created_at = meta.created_at.unwrap_or_default();

                // Filter by namespace
                if ns != namespace {
                    return None;
                }
                // Filter by session if requested
                if let Some(ref filter_sid) = session_filter {
                    if sid.as_deref() != Some(filter_sid.as_str()) {
                        return None;
                    }
                }

                Some(MemoryEntry {
                    id: h.frame_id.to_string(),
                    key,
                    content: h.text,
                    category: match cat_str.as_str() {
                        "core" => MemoryCategory::Core,
                        "daily" => MemoryCategory::Daily,
                        "conversation" => MemoryCategory::Conversation,
                        other => MemoryCategory::Custom(other.to_string()),
                    },
                    timestamp: created_at,
                    session_id: sid,
                    score: h.score.map(|s| s as f64),
                    namespace: ns,
                    importance: None,
                    superseded_by: None,
                })
            })
            .take(limit)
            .collect();

        Ok(entries)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let entries = self
            .recall(&format!("[key={key}]"), 5, None, None, None)
            .await?;
        Ok(entries.into_iter().find(|e| e.key == key))
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let query = match category {
            Some(cat) => format!("[cat={cat}]"),
            None => "*".to_string(),
        };
        self.recall(&query, 500, session_id, None, None).await
    }

    /// Memvid is append-only. We store a tombstone frame and mark the key as superseded.
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let existing = self.get(key).await?;
        if existing.is_none() {
            return Ok(false);
        }

        let tombstone = format!("[DELETED] key={key}");
        let opts = PutOptions {
            title: Some(format!("DELETED:{key}")),
            uri: Some(format!("mv2://memory/deleted/{key}")),
            extract_triplets: false,
            extra_metadata: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("key".to_string(), key.to_string());
                m.insert("deleted".to_string(), "true".to_string());
                m.insert("namespace".to_string(), self.namespace.clone());
                m
            },
            ..PutOptions::default()
        };

        let inner = self.inner.clone();
        let bytes = tombstone.into_bytes();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut mem = inner.lock();
            mem.put_bytes_with_options(&bytes, opts)?;
            mem.commit()?;
            Ok(())
        })
        .await
        .context("spawn_blocking forget")??;

        Ok(true)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let entries = self.list(None, None).await?;
        Ok(entries.len())
    }

    async fn health_check(&self) -> bool {
        self.path.exists()
    }
}
