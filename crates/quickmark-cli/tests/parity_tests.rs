//! Parity suite for the markdownlint-cli2 fixtures migrated into `test-samples/parity`.
//!
//! Each fixture was linted with markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) using
//! `noInlineConfig` and every rule at its default. The result was checked in next to each fixture
//! as `<fixture>.expected` — one `RULE:LINE` per violation, sorted. Fixtures with no violations get
//! an empty `.expected`, so "clean" is asserted too.
//!
//! This test re-lints every fixture with `qmark` and compares the two per rule. Line numbers are
//! deliberately not compared: the two tools disagree on which line to attribute a violation to for
//! some rules (markdownlint reports a missing blank line after a list on the line following it,
//! quickmark on the list's first line). Counts per rule per file is the invariant that matters.
//!
//! Known gaps are recorded in `BASELINE` as the maximum number of fixtures allowed to disagree for
//! a rule. Shrink an entry when a rule is fixed; the test reports any rule that beat its baseline
//! so the entry can be tightened.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Rules allowed to disagree, and on how many fixtures, before the suite fails. Every entry is a
/// known quickmark bug — shrink it as the rule is fixed, and never grow it.
const BASELINE: &[(&str, usize)] = &[("MD019", 47), ("MD022", 2), ("MD041", 4)];

fn parity_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-samples/parity")
        .canonicalize()
        .expect("test-samples/parity should exist")
}

fn is_fixture(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown"))
}

fn collect_fixtures(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for entry in entries {
        if entry.is_dir() {
            collect_fixtures(&entry, out);
        } else if is_fixture(&entry) {
            out.push(entry);
        }
    }
}

/// Parse one `qmark` diagnostic line into (relative path, line number, rule id).
fn parse_diagnostic(line: &str) -> Option<(String, usize, String)> {
    let rest = line
        .strip_prefix("ERR: ")
        .or_else(|| line.strip_prefix("WARN: "))?;
    // The rule id is the last space-separated token's prefix, which keeps paths containing spaces
    // or colons intact.
    let at = rest.rfind(" MD")?;
    let rule = rest[at + 1..].split('/').next()?.to_string();
    let mut parts = rest[..at].rsplitn(3, ':');
    let _column = parts.next()?;
    let line_number = parts.next()?.parse().ok()?;
    Some((parts.next()?.to_string(), line_number, rule))
}

/// Parse a checked-in `.expected` file into per-rule counts.
fn parse_expected(path: &Path) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    if let Ok(body) = std::fs::read_to_string(path) {
        for line in body.lines() {
            if let Some((rule, _line)) = line.trim().rsplit_once(':') {
                *counts.entry(rule.to_string()).or_insert(0) += 1;
            }
        }
    }
    counts
}

#[test]
fn matches_markdownlint_on_migrated_fixtures() {
    let root = parity_root();
    let config = root.join("quickmark.toml");
    assert!(config.is_file(), "missing {}", config.display());

    let mut absolute = Vec::new();
    collect_fixtures(&root, &mut absolute);
    assert!(
        absolute.len() > 300,
        "expected the migrated markdownlint-cli2 fixtures, found {}",
        absolute.len()
    );

    // Run qmark once for the whole corpus, with paths relative to the parity root so the
    // diagnostics can be matched back to their fixture.
    let relative: Vec<String> = absolute
        .iter()
        .map(|p| {
            p.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    let output = assert_cmd::Command::cargo_bin("qmark")
        .expect("qmark binary")
        .current_dir(&root)
        .env("QUICKMARK_CONFIG", &config)
        .args(&relative)
        .output()
        .expect("failed to run qmark");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut actual: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    // qmark writes diagnostics to stderr and only the tally to stdout; scan both so the suite does
    // not silently pass if that is ever changed.
    for line in stderr.lines().chain(stdout.lines()) {
        if let Some((path, _line_number, rule)) = parse_diagnostic(line) {
            *actual.entry(path).or_default().entry(rule).or_insert(0) += 1;
        }
    }

    let linted: usize = actual.values().map(BTreeMap::len).sum();
    assert!(
        linted > 0,
        "qmark reported no diagnostics at all; exit={:?}\nstderr:\n{}",
        output.status.code(),
        stderr
    );

    // Compare per fixture, per rule.
    let mut mismatching: BTreeMap<String, usize> = BTreeMap::new();
    let mut examples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut beaten = Vec::new();

    for fixture in &relative {
        let expected = parse_expected(&root.join(format!("{fixture}.expected")));
        let got = actual.get(fixture).cloned().unwrap_or_default();

        let mut rules: Vec<String> = expected.keys().cloned().collect();
        for rule in got.keys() {
            if !rules.contains(rule) {
                rules.push(rule.clone());
            }
        }

        for rule in rules {
            let want = expected.get(&rule).copied().unwrap_or(0);
            let have = got.get(&rule).copied().unwrap_or(0);
            if want == have {
                continue;
            }
            *mismatching.entry(rule.clone()).or_insert(0) += 1;
            let slot = examples.entry(rule.clone()).or_default();
            if slot.len() < 3 {
                slot.push(format!("{fixture} (markdownlint {want}, qmark {have})"));
            }
        }
    }

    let allowed: BTreeMap<&str, usize> = BASELINE.iter().copied().collect();
    let mut failures = Vec::new();
    for (rule, count) in &mismatching {
        let limit = allowed.get(rule.as_str()).copied().unwrap_or(0);
        if *count > limit {
            failures.push(format!(
                "{rule}: {count} fixture(s) disagree (baseline {limit})\n{}",
                examples[rule]
                    .iter()
                    .map(|e| format!("    {e}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        } else if *count < limit {
            beaten.push(format!(
                "{rule}: {count} < baseline {limit} — tighten BASELINE"
            ));
        }
    }
    for rule in allowed.keys() {
        if !mismatching.contains_key(*rule) {
            beaten.push(format!("{rule}: 0 disagreements — remove from BASELINE"));
        }
    }

    let total: usize = mismatching.values().sum();
    println!(
        "parity: {} fixtures, {total} rule/fixture disagreements",
        relative.len()
    );
    for (rule, count) in &mismatching {
        println!(
            "  {rule}: {count} (baseline {})",
            allowed.get(rule.as_str()).unwrap_or(&0)
        );
    }
    for line in &beaten {
        println!("  IMPROVED {line}");
    }

    assert!(
        failures.is_empty(),
        "quickmark disagrees with markdownlint beyond the recorded baseline:\n\n{}\n\n\
         Run with --nocapture for the full breakdown.",
        failures.join("\n")
    );
}
