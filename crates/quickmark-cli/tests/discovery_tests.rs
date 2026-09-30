//! File-discovery scenarios ported from markdownlint-cli2's `test/` suite.
//!
//! Only the scenarios that depend purely on how CLI arguments become a file list are portable:
//! `dot` (a directory argument recurses and picks up every markdown extension) and `dotfiles`
//! (hidden files and directories are included). The rest of markdownlint-cli2's discovery scenarios
//! — `globs`, `globs-and-args`, `globs-and-ignores`, `ignores`, `dotfiles-nested`, `gitignore`,
//! `gitignore-root-only`, `config-files`, `extension-scenario-*` — are driven by config-file
//! properties (`globs`, `ignores`, `gitignore`, `frontMatter`), by `--config`/`--configPointer`, or
//! by custom output formatters. quickmark has none of those, so there is nothing to port until the
//! CLI grows them.
//!
//! Every fixture file below contains `text` and nothing else, which trips MD041 exactly once, so the
//! set of paths appearing in the output is the set of files that were linted.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use assert_cmd::Command;
use assert_fs::TempDir;

/// Lay down `text\n`, which violates MD041 and nothing else we care about here.
fn write(root: &Path, relative: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "text\n").unwrap();
}

/// Run qmark from `root` with the given arguments and return the set of reported paths, normalised
/// to forward slashes with any leading `./` stripped.
fn linted(root: &Path, args: &[&str]) -> BTreeSet<String> {
    let output = Command::cargo_bin("qmark")
        .unwrap()
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();

    // Diagnostics go to stderr; stdout only carries the tally.
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("ERR: ").or_else(|| line.strip_prefix("WARN: "))?;
            let at = rest.rfind(" MD")?;
            let mut parts = rest[..at].rsplitn(3, ':');
            parts.next()?; // column
            parts.next()?; // line
            let path = parts.next()?.replace('\\', "/");
            Some(path.strip_prefix("./").unwrap_or(&path).to_string())
        })
        .collect()
}

/// The `dot` scenario: a directory argument recurses and lints every markdown extension, while
/// leaving other files alone.
#[test]
fn directory_argument_recurses_and_filters_by_extension() {
    let root = TempDir::new().unwrap();
    write(root.path(), "viewme.md");
    write(root.path(), "dir/about.md");
    write(root.path(), "dir/subdir/info.markdown");
    fs::write(root.path().join("about.txt"), "text\n").unwrap();

    let got = linted(root.path(), &["."]);

    assert_eq!(
        got,
        ["dir/about.md", "dir/subdir/info.markdown", "viewme.md"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

/// The `dotfiles` scenario: hidden files and hidden directories are linted, not skipped.
#[test]
fn hidden_files_and_directories_are_included() {
    let root = TempDir::new().unwrap();
    write(root.path(), ".viewme.md");
    write(root.path(), ".dir/.subdir/.info.md");
    write(root.path(), "visible.md");

    let got = linted(root.path(), &["."]);

    assert_eq!(
        got,
        [".dir/.subdir/.info.md", ".viewme.md", "visible.md"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

/// Every accepted extension must be picked up by a directory walk, an explicit path and a glob
/// alike. The walk used to go through `ignore`'s built-in markdown type, which is case-sensitive
/// and also matches `.mdx` and `.mdwn`, so the three branches disagreed.
#[test]
fn all_three_argument_forms_accept_the_same_extensions() {
    for ext in ["md", "markdown", "mdown", "mkd", "mkdn", "MD", "Markdown"] {
        let root = TempDir::new().unwrap();
        let name = format!("file.{ext}");
        write(root.path(), &name);

        let expected: BTreeSet<String> = [name.clone()].into_iter().collect();
        assert_eq!(linted(root.path(), &["."]), expected, "dir walk .{ext}");
        assert_eq!(
            linted(root.path(), &[&name]),
            expected,
            "explicit path .{ext}"
        );
        assert_eq!(
            linted(root.path(), &[&format!("*.{ext}")]),
            expected,
            "glob .{ext}"
        );
    }
}

/// Extensions outside quickmark's set are not linted. `.mdx` in particular is picked up by
/// `ignore`'s markdown type but is not markdownlint's default, so it must not sneak in via a walk.
#[test]
fn non_markdown_extensions_are_never_linted() {
    let root = TempDir::new().unwrap();
    for name in ["notes.txt", "component.mdx", "page.mdwn", "README"] {
        fs::write(root.path().join(name), "text\n").unwrap();
    }
    write(root.path(), "real.md");

    let got = linted(root.path(), &["."]);

    assert_eq!(got, ["real.md"].into_iter().map(String::from).collect());
}

/// A directory walk honours .gitignore, matching markdownlint-cli2's opt-in `gitignore` behaviour.
#[test]
fn directory_walk_respects_gitignore() {
    let root = TempDir::new().unwrap();
    // The `ignore` crate only reads .gitignore inside a git repository.
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(root.path().join(".gitignore"), "skipped.md\ndir/\n").unwrap();
    write(root.path(), "kept.md");
    write(root.path(), "skipped.md");
    write(root.path(), "dir/nested.md");

    let got = linted(root.path(), &["."]);

    assert_eq!(got, ["kept.md"].into_iter().map(String::from).collect());
}

/// A glob that matches nothing is not an error, so scripting `qmark '**/*.md'` stays usable.
#[test]
fn glob_with_no_matches_exits_zero() {
    let root = TempDir::new().unwrap();
    write(root.path(), "real.md");

    let output = Command::cargo_bin("qmark")
        .unwrap()
        .current_dir(root.path())
        .arg("no-such-*.md")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("ERR: "), "unexpected diagnostics: {stderr}");
}
