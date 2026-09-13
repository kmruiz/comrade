//! Project instructions loaded from the working directory's `AGENTS.md`.
//!
//! `AGENTS.md` is the cross-tool convention for telling a coding agent how a
//! repository wants to be worked in. Comrade reads the one at the project root
//! and folds it into the system prompt, so the model follows the repository's
//! own rules (build commands, style, guardrails) from the first turn.

use std::path::Path;

/// Read the project's `AGENTS.md` (the working-directory root only) and return
/// its trimmed contents. `None` when the file is absent or blank.
pub fn load_project_instructions(project_root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(project_root.join("AGENTS.md")).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "comrade-instructions-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn reads_and_trims_agents_md() {
        let dir = tmp_dir();
        std::fs::write(dir.join("AGENTS.md"), "  use tabs  \n").unwrap();
        assert_eq!(
            load_project_instructions(&dir),
            Some("use tabs".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absent_or_blank_agents_md_yields_none() {
        let dir = tmp_dir();
        assert_eq!(load_project_instructions(&dir), None);
        std::fs::write(dir.join("AGENTS.md"), "   \n\n").unwrap();
        assert_eq!(load_project_instructions(&dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
