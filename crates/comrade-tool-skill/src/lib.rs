//! Claude-format skills, exposed to the agent as tools.
//!
//! A *skill* is a directory `<name>/SKILL.md` whose file begins with an optional
//! YAML frontmatter (`name`, `description`) followed by a markdown body of
//! instructions (possibly referencing bundled files next to it). Skills are
//! discovered under Comrade's own `.comrade/skills` and the common Claude places
//! (`.claude/skills` in the project, `~/.claude/skills` for the user).
//!
//! Each discovered skill becomes one tool named `skill_<name>`: the model sees
//! the skill's description in the tool list and *invokes* the tool to load the
//! full SKILL.md body on demand (progressive disclosure). The registry wiring
//! lives in the TUI (see `comrade_tool_skill::all`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use comrade_tool::{Tool, ToolContext, ToolSpec};
use serde_json::{Value, json};

/// One parsed skill: its identity, the one-line description shown in the tool
/// list, the full markdown body returned on invocation, and its directory (so
/// bundled scripts/resources can be read with the `fs_*` tools).
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub dir: PathBuf,
}

/// Parse one `SKILL.md`. `dir` is the skill's directory, used as the name
/// fallback. The workspace carries no YAML crate, so the frontmatter is parsed
/// by hand: a leading `---` fence, `key: value` lines, a closing `---`.
pub fn parse_skill_md(dir: &Path, content: &str) -> Option<Skill> {
    let (front, body) = split_frontmatter(content);
    let mut name = String::new();
    let mut description = String::new();
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" => name = value,
            "description" => description = value,
            _ => {}
        }
    }
    if name.is_empty() {
        name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
    }
    if name.is_empty() {
        return None;
    }
    if description.is_empty() {
        description = body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
    }
    Some(Skill {
        name,
        description,
        body,
        dir: dir.to_path_buf(),
    })
}

/// Split `content` into (frontmatter, body). The frontmatter is the text between
/// a leading `---` line and the next lone `---`; without a leading fence the
/// frontmatter is empty and the whole content is the body.
fn split_frontmatter(content: &str) -> (String, String) {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return (String::new(), content.to_string());
    }
    // `lines[1..]` may hold the closing fence; position() is relative to it.
    if let Some(rel) = lines[1..].iter().position(|l| l.trim() == "---") {
        let end = rel + 1; // absolute index of the closing fence
        let front = lines[1..end].join("\n");
        let body = lines[end + 1..].join("\n");
        (front, body)
    } else {
        // No closing fence: not a frontmatter block after all.
        (String::new(), content.to_string())
    }
}

/// Strip one pair of matching surrounding quotes from a frontmatter value.
fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Discover skills across `dirs` in order. For each directory every immediate
/// subdirectory containing a `SKILL.md` becomes a skill; on a name collision the
/// earlier directory wins. The result is sorted by skill name.
pub fn discover_in(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut subs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subs.sort();
        for sub in subs {
            let Ok(content) = std::fs::read_to_string(sub.join("SKILL.md")) else {
                continue;
            };
            if let Some(skill) = parse_skill_md(&sub, &content)
                && seen.insert(skill.name.clone())
            {
                out.push(skill);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Discover skills from `.comrade/skills` (Comrade's default) and the common
/// Claude places: `<root>/.claude/skills`, then `$HOME/.comrade/skills` and
/// `$HOME/.claude/skills` for the user. Project directories take precedence over
/// personal ones on a name collision.
pub fn discover(project_root: &Path) -> Vec<Skill> {
    let mut dirs = vec![
        project_root.join(".comrade").join("skills"),
        project_root.join(".claude").join("skills"),
    ];
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home = PathBuf::from(home);
        dirs.push(home.join(".comrade").join("skills"));
        dirs.push(home.join(".claude").join("skills"));
    }
    discover_in(&dirs)
}

/// Every skill under `project_root` as an agent tool, sorted by name.
pub fn all(project_root: &Path) -> Vec<Box<dyn Tool>> {
    discover(project_root)
        .into_iter()
        .map(|s| Box::new(SkillTool::new(s)) as Box<dyn Tool>)
        .collect()
}

/// One skill exposed as a tool. Invoking it returns the SKILL.md body.
struct SkillTool {
    spec: ToolSpec,
    body: String,
    dir: PathBuf,
}

impl SkillTool {
    fn new(skill: Skill) -> Self {
        let name = format!("skill_{}", skill.name);
        let lead = if skill.description.is_empty() {
            skill.name.clone()
        } else {
            skill.description.clone()
        };
        let description = format!("{lead} — invoke this skill to load its full instructions.");
        let spec = ToolSpec {
            name,
            description,
            json_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        };
        Self {
            spec,
            body: skill.body,
            dir: skill.dir,
        }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<String> {
        let title = self
            .spec
            .name
            .strip_prefix("skill_")
            .unwrap_or(&self.spec.name);
        Ok(format!(
            "# Skill: {title}\n\n{body}\n\n---\nSkill directory: {dir} — read files under it with the fs tools if this skill references bundled scripts or resources.",
            body = self.body.trim(),
            dir = self.dir.display(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("comrade-skill-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_skill(root: &Path, name: &str, content: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let dir = tmp_dir();
        let content = "---\nname: pdf\ndescription: Fill PDF forms.\n---\n# Body\ndo x\n";
        let s = parse_skill_md(&dir, content).unwrap();
        assert_eq!(s.name, "pdf");
        assert_eq!(s.description, "Fill PDF forms.");
        assert!(s.body.contains("# Body"), "body was {:?}", s.body);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unquotes_description_and_falls_back_without_frontmatter() {
        let dir = tmp_dir();
        let s = parse_skill_md(
            &dir,
            "---\nname: git-flow\ndescription: \"Tidy git history.\"\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(s.name, "git-flow");
        assert_eq!(s.description, "Tidy git history.");
        // No frontmatter: name from the directory, description from the first body line.
        let d2 = dir.join("my-skill");
        let s2 = parse_skill_md(&d2, "Just some instructions.\nmore\n").unwrap();
        assert_eq!(s2.name, "my-skill");
        assert_eq!(s2.description, "Just some instructions.");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discovers_sorted_and_first_dir_wins() {
        let a = tmp_dir();
        let b = tmp_dir();
        write_skill(&a, "bbb", "---\nname: bbb\ndescription: B.\n---\nbody\n");
        write_skill(&a, "aaa", "---\nname: aaa\ndescription: A.\n---\nbody\n");
        // Same name in b: a's definition must win (dirs are precedence-ordered).
        write_skill(
            &b,
            "aaa",
            "---\nname: aaa\ndescription: from b.\n---\nbody\n",
        );
        let found = discover_in(&[a.clone(), b.clone()]);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["aaa", "bbb"]);
        assert_eq!(found[0].description, "A.");
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn all_exposes_skill_prefixed_tools() {
        let root = tmp_dir();
        write_skill(
            &root.join(".comrade").join("skills"),
            "zeta",
            "---\nname: zeta\ndescription: Z.\n---\nb\n",
        );
        write_skill(
            &root.join(".claude").join("skills"),
            "alpha",
            "---\nname: alpha\ndescription: A.\n---\nb\n",
        );
        let tools = all(&root);
        let names: Vec<String> = tools.iter().map(|t| t.spec().name.clone()).collect();
        // $HOME may contribute more skills; assert our two are present and ordered.
        let alpha = names.iter().position(|n| n == "skill_alpha");
        let zeta = names.iter().position(|n| n == "skill_zeta");
        assert!(alpha.is_some() && zeta.is_some(), "names: {names:?}");
        assert!(alpha < zeta, "skill_alpha should sort before skill_zeta");
        std::fs::remove_dir_all(&root).ok();
    }
}
