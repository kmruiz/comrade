//! Cargo project object model: workspace/project + subprojects (members),
//! dependencies, and task aliases — the Comrade analogue of a Maven POM.
//!
//! Parsing is deliberately manifest-only (no `cargo metadata`), so it works
//! offline and never touches the network. Cargo's own resolution is still the
//! source of truth when running tasks.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use toml::Value;

/// A single Cargo package (the root project, or one workspace member).
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    /// Directory of the package relative to the workspace root (`""` for the
    /// root package, otherwise like `crates/foo`).
    pub rel_dir: String,
    /// Path to its Cargo.toml relative to the workspace root.
    pub manifest_rel: String,
    pub version: Option<String>,
    pub edition: Option<String>,
    pub description: Option<String>,
    /// Dependency names by kind.
    pub deps_normal: Vec<String>,
    pub deps_dev: Vec<String>,
    pub deps_build: Vec<String>,
}

impl Package {
    fn dep_counts(&self) -> (usize, usize, usize) {
        (
            self.deps_normal.len(),
            self.deps_dev.len(),
            self.deps_build.len(),
        )
    }
}

/// A cargo alias from `.cargo/config.toml`, e.g. `t = "test"`.
#[derive(Debug, Clone)]
pub struct Alias {
    pub name: String,
    /// Expansion as given (a cargo subcommand invocation, or a shell command
    /// when it starts with `!`).
    pub expansion: String,
}

/// The parsed project object model.
#[derive(Debug, Clone)]
pub struct ProjectModel {
    /// Workspace/root directory.
    pub root: PathBuf,
    /// The package declared at the root, if the root manifest is not virtual.
    pub root_package: Option<Package>,
    /// True when the root Cargo.toml is a virtual workspace manifest.
    pub is_virtual: bool,
    /// Workspace member packages (subprojects).
    pub subprojects: Vec<Package>,
    /// Names from `[workspace.dependencies]`.
    pub workspace_deps: Vec<String>,
    /// Aliases loaded from `.cargo/config.toml`.
    pub aliases: Vec<Alias>,
}

/// Standard Cargo verbs surfaced as tasks.
pub const CARGO_VERBS: &[&str] = &[
    "build", "run", "check", "test", "clippy", "fmt", "doc", "bench", "release",
];

/// Load the Cargo POM for `root` (the workspace/project root). Errors when the
/// directory does not contain a `Cargo.toml`.
pub fn load(root: &Path) -> Result<ProjectModel> {
    let manifest = root.join("Cargo.toml");
    if !manifest.is_file() {
        anyhow::bail!("no Cargo.toml at {} (not a Cargo project)", root.display());
    }
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("cannot read {}", manifest.display()))?;
    let value: Value =
        toml::from_str(&text).with_context(|| format!("cannot parse {}", manifest.display()))?;

    let is_virtual = value.get("package").is_none();
    let root_package = if value.get("package").is_some() {
        Some(parse_package(&value, "", "Cargo.toml")?)
    } else {
        None
    };

    let workspace = value.get("workspace");
    let members: Vec<String> = workspace
        .and_then(|w| w.get("members"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    let workspace_deps: Vec<String> = workspace
        .and_then(|w| w.get("dependencies"))
        .and_then(Value::as_table)
        .map(|t| t.keys().cloned().collect())
        .unwrap_or_default();

    let mut subprojects = Vec::new();
    for member in members {
        let dir = member.trim_start_matches("./");
        if dir.is_empty() || dir == "." {
            continue;
        }
        let sub_manifest = root.join(dir).join("Cargo.toml");
        if !sub_manifest.is_file() {
            continue;
        }
        let sub_text = std::fs::read_to_string(&sub_manifest)
            .with_context(|| format!("cannot read {}", sub_manifest.display()))?;
        let sub_value: Value = toml::from_str(&sub_text)
            .with_context(|| format!("cannot parse {}", sub_manifest.display()))?;
        if sub_value.get("package").is_some() {
            let rel_manifest = format!("{dir}/Cargo.toml");
            let mut p = parse_package(&sub_value, dir, &rel_manifest)?;
            p.deps_normal.sort();
            p.deps_dev.sort();
            p.deps_build.sort();
            subprojects.push(p);
        }
    }
    subprojects.sort_by(|a, b| a.name.cmp(&b.name));

    let mut workspace_deps = workspace_deps;
    workspace_deps.sort();

    Ok(ProjectModel {
        root: root.to_path_buf(),
        root_package,
        is_virtual,
        subprojects,
        workspace_deps,
        aliases: load_aliases(root),
    })
}

/// Read `[alias]` from `<root>/.cargo/config.toml` (or the legacy `config`).
fn load_aliases(root: &Path) -> Vec<Alias> {
    for name in ["config.toml", "config"] {
        let path = root.join(".cargo").join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = text.parse::<Value>() else {
            continue;
        };
        if let Some(alias) = value.get("alias").and_then(Value::as_table) {
            let mut out = Vec::new();
            for (k, v) in alias {
                let expansion = match v {
                    Value::String(s) => s.clone(),
                    Value::Array(items) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => continue,
                };
                out.push(Alias {
                    name: k.clone(),
                    expansion,
                });
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            return out;
        }
    }
    Vec::new()
}

fn parse_package(v: &Value, rel_dir: &str, manifest_rel: &str) -> Result<Package> {
    let package = v.get("package").context("no [package] table")?;
    let name = package
        .get("name")
        .and_then(Value::as_str)
        .context("package is missing `name`")?
        .to_string();
    let str_field = |key: &str| package.get(key).and_then(Value::as_str).map(str::to_string);
    let dep_names = |key: &str| {
        v.get(key)
            .and_then(Value::as_table)
            .map(|t| t.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    let mut normal = dep_names("dependencies");
    normal.sort();
    let mut dev = dep_names("dev-dependencies");
    dev.sort();
    let mut build = dep_names("build-dependencies");
    build.sort();

    Ok(Package {
        name,
        rel_dir: rel_dir.to_string(),
        manifest_rel: manifest_rel.to_string(),
        version: str_field("version"),
        edition: str_field("edition"),
        description: str_field("description"),
        deps_normal: normal,
        deps_dev: dev,
        deps_build: build,
    })
}

/// Render a compact, human/LLM readable POM. Counts first, names capped, so
/// the output stays small even for large workspaces.
pub fn render(model: &ProjectModel) -> String {
    let mut out = String::new();

    let project_name = model
        .root_package
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| {
            model
                .root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "cargo-project".into())
        });

    let kind = if model.is_virtual {
        "workspace"
    } else {
        "package"
    };
    out.push_str(&format!(
        "Project: {project_name}  (cargo, {kind}, project root)\n"
    ));
    if let Some(p) = &model.root_package {
        let (n, d, b) = p.dep_counts();
        out.push_str(&format!(
            "  manifest: Cargo.toml  version: {}  edition: {}\n",
            p.version.as_deref().unwrap_or("?"),
            p.edition.as_deref().unwrap_or("?")
        ));
        out.push_str(&format!("  dependencies: {n} normal, {d} dev, {b} build\n"));
        if let Some(desc) = &p.description {
            out.push_str(&format!("  description: {desc}\n"));
        }
        out.push_str(&format!("    {}\n", summarize_deps(p)));
    } else {
        out.push_str("  virtual manifest (aggregates subprojects; no root package)\n");
    }
    if !model.workspace_deps.is_empty() {
        out.push_str(&format!(
            "  workspace.dependencies: {}\n",
            capped(&model.workspace_deps, 30)
        ));
    }

    if !model.subprojects.is_empty() {
        out.push_str(&format!("Subprojects ({}):\n", model.subprojects.len()));
        for (i, p) in model.subprojects.iter().enumerate() {
            let (n, d, b) = p.dep_counts();
            out.push_str(&format!("  {}. {}  @{}", i + 1, p.name, p.rel_dir));
            if let Some(v) = &p.version {
                out.push_str(&format!(" v{v}"));
            }
            out.push('\n');
            if let Some(desc) = &p.description {
                let mut d = desc.clone();
                if d.chars().count() > 90 {
                    d = d.chars().take(90).collect::<String>() + "…";
                }
                out.push_str(&format!("     {d}\n"));
            }
            out.push_str(&format!(
                "     deps: {n} normal, {d} dev, {b} build  {}\n",
                summarize_deps(p)
            ));
        }
    } else if !model.is_virtual {
        out.push_str("Subprojects: none (single crate)\n");
    }

    out.push_str(&format!(
        "Tasks (cargo verbs, run with run_task task=\"<name>\"): {}\n",
        CARGO_VERBS.join(", ")
    ));
    if model.aliases.is_empty() {
        out.push_str("Aliases: none (add [alias] to .cargo/config.toml)\n");
    } else {
        out.push_str("Aliases:\n");
        for a in &model.aliases {
            out.push_str(&format!("  {} -> cargo {}\n", a.name, a.expansion));
        }
    }
    out
}

fn summarize_deps(p: &Package) -> String {
    let mut all = Vec::new();
    all.extend(p.deps_normal.iter().cloned());
    all.extend(p.deps_dev.iter().map(|d| format!("{d} (dev)")));
    all.extend(p.deps_build.iter().map(|d| format!("{d} (build)")));
    if all.is_empty() {
        return String::new();
    }
    format!("[{capped}]", capped = capped(&all, 20))
}

fn capped(names: &[String], limit: usize) -> String {
    let mut out = String::new();
    let shown: Vec<&str> = names.iter().take(limit).map(String::as_str).collect();
    out.push_str(&shown.join(", "));
    if names.len() > limit {
        out.push_str(&format!(" … and {} more", names.len() - limit));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "comrade-pom-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_virtual_workspace_with_members() {
        let root = scratch();
        std::fs::write(
            root.join("Cargo.toml"),
            r#"
[workspace]
resolver = "3"
members = ["crates/app", "crates/lib"]

[workspace.dependencies]
serde = "1"
anyhow = "1"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("crates/app/src")).unwrap();
        std::fs::create_dir_all(root.join("crates/lib/src")).unwrap();
        std::fs::write(
            root.join("crates/app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = { workspace = true }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("crates/lib/Cargo.toml"),
            "[package]\nname = \"lib\"\nversion = \"0.2.0\"\nedition = \"2021\"\ndescription = \"a helper crate\"\n\n[dependencies]\nserde = \"1\"\n\n[dev-dependencies]\npretty = \"0.2\"\n",
        )
        .unwrap();

        let m = load(&root).unwrap();
        assert!(m.is_virtual);
        assert!(m.root_package.is_none());
        assert_eq!(m.subprojects.len(), 2);
        assert_eq!(m.workspace_deps, vec!["anyhow", "serde"]);

        let app = &m.subprojects[0];
        assert_eq!(app.name, "app");
        assert_eq!(app.deps_normal, vec!["serde"]);
        assert_eq!(app.rel_dir, "crates/app");

        let lib = &m.subprojects[1];
        assert_eq!(lib.deps_dev, vec!["pretty"]);
        assert_eq!(lib.description.as_deref(), Some("a helper crate"));
        let rendered = render(&m);
        assert!(rendered.contains("Subprojects (2):"), "{rendered}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parses_single_package_project() {
        let root = scratch();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"solo\"\nversion = \"0.0.1\"\nedition = \"2021\"\n\n[dependencies]\nanyhow = \"1\"\n",
        )
        .unwrap();
        let m = load(&root).unwrap();
        assert!(!m.is_virtual);
        assert_eq!(m.root_package.as_ref().unwrap().name, "solo");
        assert!(m.subprojects.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loads_aliases_from_cargo_config() {
        let root = scratch();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[alias]\nt = \"test\"\nci = [\"check\", \"--all-targets\"]\n",
        )
        .unwrap();
        let m = load(&root).unwrap();
        assert_eq!(m.aliases.len(), 2);
        assert_eq!(m.aliases[0].name, "ci");
        assert_eq!(m.aliases[0].expansion, "check --all-targets");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn errors_when_not_a_cargo_project() {
        let root = scratch();
        assert!(load(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
