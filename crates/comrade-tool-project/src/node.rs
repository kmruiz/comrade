//! Node/npm project object model: `package.json` + `workspaces` (subprojects),
//! dependencies and npm scripts — the Node analogue of the Cargo [`crate::pom`].
//!
//! Parsing is deliberately manifest-only (no `npm ls`/network), so it works
//! offline. npm itself stays the source of truth when running tasks.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde_json::Value;

/// One npm script (a `scripts` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeScript {
    pub name: String,
    pub command: String,
}

/// A parsed `package.json` (the root project or a workspace package).
#[derive(Debug, Clone)]
pub struct NodePackage {
    pub name: String,
    /// Directory relative to the root (`""` for the root package).
    pub rel_dir: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub private: bool,
    /// `dependencies` + `peerDependencies` + `optionalDependencies`.
    pub deps: Vec<String>,
    pub dev_deps: Vec<String>,
    pub scripts: Vec<NodeScript>,
}

impl NodePackage {
    /// `(normal, dev)` dependency counts.
    fn dep_counts(&self) -> (usize, usize) {
        (self.deps.len(), self.dev_deps.len())
    }
}

/// The parsed Node POM.
#[derive(Debug, Clone)]
pub struct NodeModel {
    pub root_package: NodePackage,
    /// The raw `workspaces` globs, if any.
    pub workspace_globs: Vec<String>,
    /// Workspace packages resolved from the globs (best effort).
    pub subprojects: Vec<NodePackage>,
}

/// Load the Node POM for `root`. Errors when there is no `package.json`.
pub fn load(root: &Path) -> Result<NodeModel> {
    let manifest = root.join("package.json");
    if !manifest.is_file() {
        anyhow::bail!("no package.json at {} (not a Node project)", root.display());
    }
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("cannot read {}", manifest.display()))?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("cannot parse {}", manifest.display()))?;

    let root_package = parse_package(&value, "")?;
    let workspace_globs = workspace_globs(&value);

    let mut subprojects = Vec::new();
    for dir in expand_workspaces(root, &workspace_globs) {
        let Ok(sub_text) = std::fs::read_to_string(dir.join("package.json")) else {
            continue;
        };
        let Ok(sub_value) = serde_json::from_str::<Value>(&sub_text) else {
            continue;
        };
        let rel = dir
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| dir.to_string_lossy().into_owned());
        if let Ok(p) = parse_package(&sub_value, &rel) {
            subprojects.push(p);
        }
    }
    subprojects.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(NodeModel {
        root_package,
        workspace_globs,
        subprojects,
    })
}

fn parse_package(v: &Value, rel_dir: &str) -> Result<NodePackage> {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if rel_dir.is_empty() {
                "project".to_string()
            } else {
                rel_dir.to_string()
            }
        });
    let str_field = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let keys = |key: &str| {
        v.get(key)
            .and_then(Value::as_object)
            .map(|o| o.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let mut deps = keys("dependencies");
    deps.extend(keys("peerDependencies"));
    deps.extend(keys("optionalDependencies"));
    deps.sort();
    deps.dedup();
    let mut dev_deps = keys("devDependencies");
    dev_deps.sort();
    dev_deps.dedup();

    let mut scripts = v
        .get("scripts")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| {
                    val.as_str().map(|s| NodeScript {
                        name: k.clone(),
                        command: s.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    scripts.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(NodePackage {
        name,
        rel_dir: rel_dir.to_string(),
        version: str_field("version"),
        description: str_field("description"),
        private: v.get("private").and_then(Value::as_bool).unwrap_or(false),
        deps,
        dev_deps,
        scripts,
    })
}

/// The `workspaces` globs, in either the array or `{ "packages": [...] }` form.
fn workspace_globs(v: &Value) -> Vec<String> {
    match v.get("workspaces") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::Object(o)) => o
            .get("packages")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Best-effort expansion of workspace globs into directories that hold a
/// `package.json`. Only a single trailing `*`/`?` segment is expanded.
fn expand_workspaces(root: &Path, globs: &[String]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for glob in globs {
        let glob = glob.trim_end_matches('/');
        if glob.is_empty() {
            continue;
        }
        if !glob.contains(['*', '?']) {
            dirs.push(root.join(glob));
            continue;
        }
        let (parent, pattern) = match glob.rsplit_once('/') {
            Some((p, pat)) => (p, pat),
            None => ("", glob),
        };
        let base = if parent.is_empty() {
            root.to_path_buf()
        } else {
            root.join(parent)
        };
        let Ok(rd) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in rd.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            if pattern == "*" || pattern == "**" || glob_match(pattern, &name) {
                dirs.push(entry.path());
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Minimal single-segment glob match supporting `*` and `?`.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn m(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => m(&p[1..], n) || (!n.is_empty() && m(p, &n[1..])),
            Some('?') => !n.is_empty() && m(&p[1..], &n[1..]),
            Some(c) => n.first() == Some(c) && m(&p[1..], &n[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    m(&p, &n)
}

/// Render a compact, model-readable POM for the Node project.
pub fn render(model: &NodeModel) -> String {
    let mut out = String::new();
    let p = &model.root_package;
    let (n, d) = p.dep_counts();
    let kind = if p.private {
        "node, private, project root"
    } else {
        "node, project root"
    };
    out.push_str(&format!("Project: {}  ({kind})\n", p.name));
    out.push_str(&format!(
        "  manifest: package.json  version: {}\n",
        p.version.as_deref().unwrap_or("?")
    ));
    out.push_str(&format!("  dependencies: {n} normal, {d} dev\n"));
    if let Some(desc) = &p.description {
        out.push_str(&format!("  description: {desc}\n"));
    }
    if !p.deps.is_empty() || !p.dev_deps.is_empty() {
        let mut names = p.deps.clone();
        names.extend(p.dev_deps.iter().cloned());
        out.push_str(&format!("    {}\n", capped(&names, 30)));
    }
    if !p.scripts.is_empty() {
        out.push_str(&format!("  scripts ({}):\n", p.scripts.len()));
        for s in &p.scripts {
            out.push_str(&format!("    {} -> {}\n", s.name, s.command));
        }
    }
    if !model.workspace_globs.is_empty() {
        out.push_str(&format!(
            "  workspaces: {}\n",
            model.workspace_globs.join(", ")
        ));
    }
    if !model.subprojects.is_empty() {
        out.push_str(&format!("Subprojects ({}):\n", model.subprojects.len()));
        for (i, sp) in model.subprojects.iter().enumerate() {
            let (n, d) = sp.dep_counts();
            out.push_str(&format!("  {}. {}  @{}", i + 1, sp.name, sp.rel_dir));
            if let Some(v) = &sp.version {
                out.push_str(&format!(" v{v}"));
            }
            out.push('\n');
            out.push_str(&format!("     deps: {n} normal, {d} dev\n"));
        }
    }
    out
}

fn capped(names: &[String], limit: usize) -> String {
    if names.len() <= limit {
        names.join(", ")
    } else {
        format!(
            "{} … (+{} more)",
            names[..limit].join(", "),
            names.len() - limit
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comrade-node-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_package_json_with_scripts_and_deps() {
        let dir = scratch("basic");
        std::fs::write(
            dir.join("package.json"),
            r#"{
              "name": "webapp",
              "version": "2.1.0",
              "description": "demo",
              "dependencies": { "react": "^18" },
              "devDependencies": { "typescript": "^5" },
              "scripts": { "build": "tsc", "test": "jest", "dev": "vite" }
            }"#,
        )
        .unwrap();
        let m = load(&dir).unwrap();
        assert_eq!(m.root_package.name, "webapp");
        assert_eq!(m.root_package.version.as_deref(), Some("2.1.0"));
        assert_eq!(m.root_package.deps, vec!["react"]);
        assert_eq!(m.root_package.dev_deps, vec!["typescript"]);
        let names: Vec<&str> = m
            .root_package
            .scripts
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["build", "dev", "test"]);
        let rendered = render(&m);
        assert!(rendered.contains("Project: webapp"), "{rendered}");
        assert!(
            rendered.contains("dependencies: 1 normal, 1 dev"),
            "{rendered}"
        );
        assert!(rendered.contains("build -> tsc"), "{rendered}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expands_workspace_globs() {
        let dir = scratch("ws");
        std::fs::write(
            dir.join("package.json"),
            r#"{ "name": "mono", "private": true, "workspaces": ["packages/*"] }"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("packages/a")).unwrap();
        std::fs::write(
            dir.join("packages/a/package.json"),
            r#"{ "name": "@mono/a", "version": "1.0.0" }"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("packages/b")).unwrap();
        std::fs::write(
            dir.join("packages/b/package.json"),
            r#"{ "name": "@mono/b" }"#,
        )
        .unwrap();
        let m = load(&dir).unwrap();
        assert_eq!(m.workspace_globs, vec!["packages/*"]);
        assert_eq!(m.subprojects.len(), 2);
        assert_eq!(m.subprojects[0].name, "@mono/a");
        assert_eq!(m.subprojects[0].rel_dir, "packages/a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_without_package_json() {
        let dir = scratch("none");
        assert!(load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
