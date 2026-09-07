use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use comrade_tool::UndoLog;

/// In-memory first-write backup log. Every captured mutation restores the file
/// to the content it had just before that mutation when undone (LIFO), so
/// consecutive edits chain correctly. Not tied to git.
pub struct MemoryUndo {
    root: PathBuf,
    entries: Mutex<Vec<(String, String)>>,
}

impl MemoryUndo {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            entries: Mutex::new(Vec::new()),
        }
    }

    pub fn entry_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

#[async_trait]
impl UndoLog for MemoryUndo {
    async fn capture(&self, path: &str, before: String) -> Result<()> {
        self.entries
            .lock()
            .unwrap()
            .push((path.to_string(), before));
        Ok(())
    }

    async fn undo_last(&self) -> Result<usize> {
        let (path, before) = {
            let mut guard = self.entries.lock().unwrap();
            guard.pop().context("nothing to undo")?
        };
        let abs = self.root.join(&path);
        if before.is_empty() {
            let _ = tokio::fs::remove_file(&abs).await;
        } else {
            if let Some(parent) = abs.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&abs, before)
                .await
                .with_context(|| format!("failed to restore {path}"))?;
        }
        Ok(self.entries.lock().unwrap().len())
    }

    async fn is_empty(&self) -> bool {
        self.entries.lock().unwrap().is_empty()
    }

    async fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}
