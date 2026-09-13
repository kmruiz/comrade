use crate::tui::Msg;
use anyhow::{Context as _, Result};
use comrade_core::ChatMessage;
use comrade_tool::PlanStep;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
pub struct SessionFile {
    pub version: u32,
    pub title: String,
    pub status: String,
    pub plan: Vec<PlanStep>,
    pub delegated: Vec<u64>,
    pub finished: Option<String>,
    pub chat: Vec<Msg>,
    pub section_collapsed: Vec<bool>,
    pub ctx_tokens: usize,
    pub ctx_budget: usize,
    pub ctx_estimated: bool,
    #[serde(default)]
    pub history: Vec<ChatMessage>,
    #[serde(default)]
    pub rollup: String,
    #[serde(default)]
    pub evicted: usize,
}

pub fn save(path: &Path, file: &SessionFile) -> Result<()> {
    let json = serde_json::to_string_pretty(file).context("serialize session")?;
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn load(path: &Path) -> Result<SessionFile> {
    let json = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&json).with_context(|| format!("parse session {}", path.display()))
}

pub fn default_path(root: &Path) -> PathBuf {
    root.join("session.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file_round_trip() {
        let file = SessionFile {
            version: 1,
            title: "t".to_string(),
            status: "".to_string(),
            plan: Vec::new(),
            delegated: Vec::new(),
            finished: None,
            chat: Vec::new(),
            section_collapsed: Vec::new(),
            ctx_tokens: 0,
            ctx_budget: 0,
            ctx_estimated: false,
            history: Vec::new(),
            rollup: String::new(),
            evicted: 0,
        };
        let path = std::env::temp_dir().join("comrade_session_test.json");
        save(&path, &file).expect("save failed");
        let loaded = load(&path).expect("load failed");
        assert_eq!(loaded.title, "t");
        assert_eq!(loaded.status, "");
        assert_eq!(loaded.version, 1);
        std::fs::remove_file(&path).ok();
    }
}
