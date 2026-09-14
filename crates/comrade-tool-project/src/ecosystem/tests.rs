use super::*;

#[test]
fn detects_cargo_and_rejects_unknown() {
    let dir = std::env::temp_dir().join(format!("comrade-eco-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // No manifest yet -> unsupported.
    assert!(detect(&dir).is_err());
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let eco = detect(&dir).unwrap();
    assert_eq!(eco.name(), "cargo");
    assert_eq!(eco.manifest(), "Cargo.toml");
    assert!(eco.supports(&dir, "check"));
    assert!(eco.supports(&dir, "test"));
    assert!(!eco.supports(&dir, "nope"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cargo_check_command_is_json() {
    let eco = Cargo;
    let sub = Some("crates/a".to_string());
    let cmd = eco
        .check_command(
            Path::new("/tmp/x"),
            &sub,
            true,
            &["--features".into(), "f".into()],
        )
        .unwrap()
        .unwrap();
    match cmd {
        CommandLine::Program { program, args } => {
            assert_eq!(program, "cargo");
            assert_eq!(args[0], "check");
            assert!(
                args.contains(&"--manifest-path=crates/a/Cargo.toml".to_string()),
                "{args:?}"
            );
            assert!(args.contains(&"--all-targets".to_string()), "{args:?}");
            assert!(args.contains(&"--features".to_string()), "{args:?}");
            assert_eq!(args.last().unwrap(), "--message-format=json");
        }
        _ => panic!("expected a program command"),
    }
}

#[test]
fn cargo_is_test_command_only_for_cargo_test() {
    let eco = Cargo;
    assert!(eco.is_test_command(&CommandLine::Program {
        program: "cargo".into(),
        args: vec!["test".into(), "--lib".into()],
    }));
    assert!(!eco.is_test_command(&CommandLine::Program {
        program: "cargo".into(),
        args: vec!["check".into()],
    }));
    assert!(!eco.is_test_command(&CommandLine::Shell {
        script: "cargo test".into(),
    }));
}

#[test]
fn parses_and_caps_json_diagnostics() {
    // Two compiler-message errors + noise; cap at 1.
    let raw = r#"{"reason":"compiler-artifact","package_id":"x"}
{"reason":"compiler-message","message":{"level":"error","message":"cannot find value `a`","code":{"code":"E0425"},"spans":[{"file_name":"src/main.rs","line_start":3,"column_start":5,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"warning","message":"unused","spans":[]}}
{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":null,"spans":[{"file_name":"src/lib.rs","line_start":9,"column_start":1,"is_primary":true}]}}
Compiling foo
"#;
    let (errors, total) = Cargo.parse_diagnostics(raw, 1);
    assert_eq!(total, 2);
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0],
        "src/main.rs:3:5: error[E0425]: cannot find value `a`"
    );

    let (all, total) = Cargo.parse_diagnostics(raw, 10);
    assert_eq!(total, 2);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1], "src/lib.rs:9:1: error: mismatched types");

    // Non-JSON output falls back to the generic error-line scan.
    let (generic, total) =
        Cargo.parse_diagnostics("Compiling x\nerror: expected `;`\nwarning: unused\n", 10);
    assert_eq!(total, 1);
    assert_eq!(generic, vec!["error: expected `;`".to_string()]);
}

#[test]
fn test_output_is_simplified() {
    let raw = "\
   Compiling comrade-core v0.1.0
    Finished `dev` profile [unoptimized + debuginfo]
     Running unittests src/lib.rs
running 12 tests
test foo ... ok
test bar ... FAILED

failures:
    bar

---- bar stdout ----
thread 'bar' panicked at src/lib.rs:10
note: run with `RUST_BACKTRACE=1` to see the stack trace

failures:
    bar
test result: FAILED. 11 passed; 1 failed; 0 ignored
";
    let out = Cargo.simplify_tests(raw);
    assert!(out.contains("test result: FAILED"));
    assert!(out.contains("11 passed"));
    assert!(out.contains("panicked at"));
    assert!(!out.contains("Compiling"));
    assert!(!out.contains("Finished"));
}

#[test]
fn failing_test_output_is_bounded_and_failure_only() {
    let mut raw = String::from(
        "running 2 tests\ntest ok_one ... ok\ntest boom ... FAILED\n\nfailures:\n\n---- boom stdout ----\n",
    );
    // A failing test that prints a lot of captured stdout.
    for i in 0..500 {
        raw.push_str(&format!("noisy stdout line {i}\n"));
    }
    raw.push_str(
            "\n---- boom stderr ----\nthread 'boom' panicked at src/lib.rs:7:5:\nboom\n\nfailures:\n    boom\ntest result: FAILED. 1 passed; 1 failed; 0 ignored\n",
        );
    let out = Cargo.simplify_tests(&raw);
    assert!(out.contains("1 failing test(s): boom"), "{out}");
    assert!(out.contains("test result: FAILED"), "{out}");
    assert!(out.contains("panicked at"), "{out}");
    assert!(
        !out.contains("ok_one"),
        "passing tests must be dropped: {out}"
    );
    assert!(
        !out.contains("noisy stdout line 499"),
        "per-test stdout must be bounded: {out}"
    );
    assert!(
        out.chars().count() <= 4000,
        "bounded: {}",
        out.chars().count()
    );
}

fn node_scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("comrade-eco-node-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn detect_all_finds_both_ecosystems_in_a_mixed_repo() {
    let dir = node_scratch("mixed");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{ "name": "x", "scripts": { "build": "tsc" } }"#,
    )
    .unwrap();
    let names: Vec<&str> = detect_all(&dir).iter().map(|e| e.name()).collect();
    assert_eq!(names, vec!["cargo", "npm"]);
    // detect() keeps the highest priority (cargo).
    assert_eq!(detect(&dir).unwrap().name(), "cargo");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn node_resolves_build_to_npm_run() {
    let dir = node_scratch("resolve");
    std::fs::write(
        dir.join("package.json"),
        r#"{ "name": "x", "scripts": { "build": "tsc", "test": "jest", "dev": "vite" } }"#,
    )
    .unwrap();
    let eco = Node;
    assert_eq!(eco.name(), "npm");
    assert!(eco.supports(&dir, "build"));
    assert!(eco.supports(&dir, "test"));
    assert!(!eco.supports(&dir, "doc"));
    let r = eco.resolve(&dir, "build", &None, &[]).unwrap();
    match r.line {
        CommandLine::Program { program, args } => {
            assert_eq!(program, "npm");
            assert_eq!(args, vec!["run", "build"]);
        }
        _ => panic!("expected a program command"),
    }
    // "run" maps onto the conventional `dev` script.
    let r = eco.resolve(&dir, "run", &None, &[]).unwrap();
    assert_eq!(r.describe, "npm run dev");
    // A subproject runs through --prefix; extra args pass after `--`.
    let r = eco
        .resolve(&dir, "test", &Some("apps/web".into()), &["--watch".into()])
        .unwrap();
    assert_eq!(r.describe, "npm --prefix apps/web run test -- --watch");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parses_tsc_diagnostics() {
    let raw = "src/App.tsx(12,5): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/x.ts(1,1): error TS1005: ';' expected.\nFound 2 errors in the same file.\n";
    let (errors, total) = Node.parse_diagnostics(raw, 1);
    assert_eq!(total, 2);
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0],
        "src/App.tsx:12:5: error TS2322: Type 'string' is not assignable to type 'number'."
    );
    // No located diagnostics -> generic fallback.
    let (g, t) = Node.parse_diagnostics("error: something broke\n", 5);
    assert_eq!(t, 1);
    assert!(g[0].contains("error: something broke"));
}

#[test]
fn node_is_test_command_and_simplifies_tests() {
    let eco = Node;
    assert!(eco.is_test_command(&CommandLine::Program {
        program: "npm".into(),
        args: vec!["test".into()],
    }));
    assert!(eco.is_test_command(&CommandLine::Program {
        program: "npm".into(),
        args: vec!["run".into(), "test".into()],
    }));
    assert!(!eco.is_test_command(&CommandLine::Program {
        program: "npm".into(),
        args: vec!["run".into(), "build".into()],
    }));
    let raw = "PASS src/a.test.ts\n  ✓ adds (2 ms)\nTest Suites: 1 passed, 1 total\nTests:       1 passed, 1 total\nrandom noise\n";
    let s = eco.simplify_tests(raw);
    assert!(s.contains("Test Suites: 1 passed"), "{s}");
    assert!(s.contains("Tests:       1 passed"), "{s}");
    assert!(!s.contains("random noise"), "{s}");
}

#[test]
fn pick_resolves_polyglot_repos() {
    let dir = node_scratch("pick");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("package.json"),
        r#"{ "name": "x", "scripts": { "build": "tsc" } }"#,
    )
    .unwrap();
    // Explicit selection wins.
    assert_eq!(pick(&dir, Some("npm"), "build").unwrap().name(), "npm");
    assert_eq!(pick(&dir, Some("cargo"), "build").unwrap().name(), "cargo");
    // An unknown ecosystem name errors.
    assert!(pick(&dir, Some("maven"), "build").is_err());
    // `build` is supported by both -> ambiguous without an explicit choice.
    assert!(pick(&dir, None, "build").is_err());
    // A verb only cargo supports resolves automatically.
    assert_eq!(pick(&dir, None, "clippy").unwrap().name(), "cargo");
    let _ = std::fs::remove_dir_all(&dir);

    // A single-ecosystem project needs no verb match.
    let cargo_only = node_scratch("pick2");
    std::fs::write(
        cargo_only.join("Cargo.toml"),
        "[package]\nname = \"y\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    assert_eq!(pick(&cargo_only, None, "build").unwrap().name(), "cargo");
    let _ = std::fs::remove_dir_all(&cargo_only);
}
