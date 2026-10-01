//! Byte-exact comparison of the comrak facade against a real tree-sitter-md parse.
//!
//! This module is the migration's only real safety net. Roughly ten assertions across 53 rules pin a
//! column value, and the CLI's parity suite deliberately discards line and column, so nothing else
//! would notice a block boundary or column that shifted by one. Everything here is `#[cfg(test)]`
//! and `tree-sitter`/`tree-sitter-md` are dev-dependencies only; the module is deleted once the
//! migration has settled.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::build;
use crate::ast::walker::Walker;
use crate::ast::KIND_NAMES;

/// One node, in the terms both trees can be compared on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Rec {
    kind: String,
    start_row: usize,
    start_col: usize,
    end_row: usize,
    end_col: usize,
    start_byte: usize,
    end_byte: usize,
    named: bool,
}

impl std::fmt::Display for Rec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} r{}:{}-r{}:{} b{}-{}{}",
            self.kind,
            self.start_row,
            self.start_col,
            self.end_row,
            self.end_col,
            self.start_byte,
            self.end_byte,
            if self.named { "" } else { " (anon)" }
        )
    }
}

/// Dumps tree-sitter-md's block tree, keeping only the node kinds the facade claims to reproduce.
///
/// Filtering by `KIND_NAMES` is what makes the comparison meaningful: tree-sitter-md also emits
/// `block_continuation`, `block_quote_marker`, `fenced_code_block_delimiter`, `info_string`,
/// `link_label`, `task_list_marker_*` and one anonymous node per punctuation character — 6.4 million
/// nodes across the vault corpus, none of which any rule reads.
fn dump_tree_sitter(source: &str) -> Option<Vec<Rec>> {
    if crate::ast::synth::LineIndex::has_bare_carriage_return(source) {
        // tree-sitter-md does not treat a bare `\r` as a line break; comrak and CommonMark do. The
        // two parses of such a document number their lines differently, so there is nothing to
        // compare. The facade follows CommonMark. Affects 4 of the 2747 vault files.
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .expect("markdown grammar");
    let tree = parser.parse(source, None)?;
    if tree.root_node().kind() == "ERROR" {
        // tree-sitter-md fails the whole document for some inputs it cannot parse — a file ending in
        // an ATX heading with no trailing newline, for instance. comrak always produces a tree, so
        // there is nothing to compare against; the caller counts these.
        return None;
    }

    let mut out = Vec::new();
    push_tree_sitter(tree.root_node(), None, true, &mut out);
    Some(out)
}

/// Whether a node is on the document-level `section` chain: the document itself, or a `section`
/// whose parent is. tree-sitter-md also wraps a *container's* children in a section when the
/// container holds an ATX heading; the facade only synthesizes document-level ones, which is all
/// MD036 — the rule's only consumer — can observe, since `is_document_level` rejects any chain
/// containing a container whether or not a nested section sits in between.
fn in_section_chain(kind: &str, parent_in_chain: bool) -> bool {
    parent_in_chain && matches!(kind, "document" | "section")
}

fn is_comparable(kind: &str, parent_kind: Option<&str>, parent_in_chain: bool) -> bool {
    match kind {
        // A bare `|` is only a table separator when its parent is a table row. tree-sitter-md also
        // emits anonymous `|` nodes inside `inline`, `code_fence_content` and `html_block`, where
        // the facade emits nothing.
        "|" => matches!(
            parent_kind,
            Some("pipe_table_header") | Some("pipe_table_row") | Some("pipe_table_delimiter_row")
        ),
        "section" => parent_in_chain,
        _ => KIND_NAMES.contains(&kind),
    }
}

fn push_tree_sitter(
    node: tree_sitter::Node<'_>,
    parent_kind: Option<&str>,
    parent_in_chain: bool,
    out: &mut Vec<Rec>,
) {
    let kind = node.kind();
    let comparable = is_comparable(kind, parent_kind, parent_in_chain);
    if comparable {
        let start = node.start_position();
        let end = node.end_position();
        out.push(Rec {
            kind: kind.to_string(),
            start_row: start.row,
            start_col: start.column,
            end_row: end.row,
            end_col: end.column,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            named: node.is_named(),
        });
    }
    let child_in_chain = in_section_chain(kind, parent_in_chain);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        push_tree_sitter(child, Some(kind), child_in_chain, out);
    }
}

fn dump_facade(source: &str) -> Vec<Rec> {
    let tree = build::parse(source);
    let mut out = Vec::with_capacity(tree.node_count());
    Walker::new(&tree).walk(|node| {
        let kind = node.kind();
        let parent_kind = node.parent().map(|parent| parent.kind());
        let parent_in_chain = node
            .parent()
            .is_none_or(|parent| in_section_chain(parent.kind(), facade_in_chain(parent)));
        if !is_comparable(kind, parent_kind, parent_in_chain) {
            return;
        }
        let start = node.start_position();
        let end = node.end_position();
        out.push(Rec {
            kind: kind.to_string(),
            start_row: start.row,
            start_col: start.column,
            end_row: end.row,
            end_col: end.column,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            named: node.is_named(),
        });
    });
    out
}

fn facade_in_chain(node: crate::ast::Node<'_>) -> bool {
    match node.kind() {
        "document" => true,
        "section" => node.parent().is_some_and(facade_in_chain),
        _ => false,
    }
}

/// Inputs where comrak and tree-sitter-md genuinely disagree and the disagreement is accepted.
///
/// This is not a place to hide bugs: every entry has to say which parser is right and why the
/// difference is tolerable, and the parity and vault runs are what confirm it is unobservable. The
/// edge-case test fails if an entry stops being needed, so this cannot silently rot.
const KNOWN_PARSER_DIFFS: &[(&str, &str)] = &[(
    "setext no trailing newline",
    "`Title\n===` with no trailing newline: CommonMark allows the underline to end the file and \
         comrak reads a setext heading, but tree-sitter-md requires the newline and leaves a \
         two-line paragraph. comrak is the spec-conformant reading. Affects the heading rules for \
         files that both end in a setext heading and lack a final newline — which MD047 already \
         reports.",
)];

/// Documents tree-sitter-md could not parse at all, where it returns an `ERROR` root and quickmark
/// therefore reports almost nothing today. comrak always produces a tree, so these files start being
/// linted properly — an intentional behaviour change, listed here so it stays a known quantity rather
/// than a surprise.
const KNOWN_TREE_SITTER_FAILURES: &[(&str, &str)] = &[(
    "heading no trailing newline",
    "tree-sitter-md fails the whole document when it ends in an ATX heading with no trailing \
         newline, producing an `ERROR` root and no `document` node, so no rule that keys on \
         `document` fires. comrak parses it normally. MD047 in particular starts reporting these \
         files, which is the correct behaviour.",
)];

/// Compares one document, returning a human-readable diff or `None` when the trees agree.
fn compare(label: &str, source: &str) -> Option<String> {
    compare_allowing(label, source, true)
}

fn compare_allowing(label: &str, source: &str, allow_known: bool) -> Option<String> {
    if allow_known && KNOWN_PARSER_DIFFS.iter().any(|&(known, _)| known == label) {
        return None;
    }
    // tree-sitter could not parse it, so there is nothing to compare.
    let expected = dump_tree_sitter(source)?;
    let actual = dump_facade(source);
    if expected == actual {
        return None;
    }

    let mut diff = format!("--- {label}\nSRC {:?}\n", truncate(source));
    let longest = expected.len().max(actual.len());
    let mut shown = 0;
    for index in 0..longest {
        let (e, a) = (expected.get(index), actual.get(index));
        if e == a {
            continue;
        }
        if shown >= 12 {
            diff.push_str("  ...\n");
            break;
        }
        shown += 1;
        diff.push_str(&format!(
            "  [{index}] tree-sitter: {}\n         facade     : {}\n",
            e.map(ToString::to_string)
                .unwrap_or_else(|| "<missing>".to_string()),
            a.map(ToString::to_string)
                .unwrap_or_else(|| "<missing>".to_string())
        ));
    }
    diff.push_str(&format!(
        "  counts: tree-sitter {} vs facade {}\n",
        expected.len(),
        actual.len()
    ));
    Some(diff)
}

/// Structural edge cases, one per synthesis recipe. These are the inputs most likely to expose a
/// span that was derived rather than measured.
const EDGE_CASES: &[(&str, &str)] = &[
    ("atx", "# H1\n"),
    ("atx closed", "# H1 #\n"),
    ("atx closed trailing ws", "# H1 #   \n"),
    ("atx empty", "##\n"),
    ("atx hashes only", "#######\n"),
    ("atx indented 3", "   ### Deep\n"),
    ("atx tab indent", "\t# H\n"),
    ("atx no space", "#H\n"),
    ("setext h1", "Title\n=====\n"),
    ("setext h2", "Title\n-----\n"),
    ("setext indented", "  Title\n  =====\n"),
    ("setext multiline", "one\ntwo\n===\n"),
    ("sections flat", "# A\ntext\n# B\ntext\n"),
    ("sections nested", "# A\ntext\n## B\ntext\n# C\n"),
    ("sections deep", "# A\n## B\n### C\n## D\n# E\n"),
    ("doc starts with heading", "# A\ntext\n"),
    ("doc starts with text", "text\n# H\n"),
    ("doc text blank heading", "text\n\n# H\n"),
    ("front matter minus", "---\ntitle: x\n---\n\n# H\n"),
    ("front matter minus only", "---\ntitle: x\n---\n"),
    ("front matter plus", "+++\ntitle: x\n+++\n\n# H\n"),
    ("paragraph", "hello world\n"),
    ("paragraph trailing ws", "hello   \n"),
    ("paragraph indented", "  hello\n"),
    ("paragraph two lines", "hello\nworld\n"),
    ("paragraph two lines trailing ws", "hello\nworld  \n"),
    ("paragraphs", "a\n\nb\n"),
    ("bullet minus", "- one\n- two\n"),
    ("bullet star", "* one\n"),
    ("bullet plus", "+ one\n"),
    ("bullet padding", "-   one\n"),
    ("ordered dot", "1. one\n2. two\n"),
    ("ordered paren", "1) one\n"),
    ("ordered padding", "1.  one\n"),
    ("ordered multi digit", "10. ten\n"),
    ("nested list", "- a\n  - b\n    - c\n"),
    ("loose list", "- a\n\n- b\n"),
    ("loose list trailing blank", "- a\n- b\n\ntext\n"),
    ("list then heading", "- a\n\n# H\n"),
    ("list indented top level", "  - a\n  - b\n"),
    ("task list", "- [x] done\n- [ ] todo\n"),
    ("empty list item", "-\n"),
    ("fenced backtick", "```rust\nlet x = 1;\n```\n"),
    ("fenced tilde", "~~~\ncode\n~~~\n"),
    ("fenced unclosed", "```\ncode\n"),
    ("fenced empty", "```\n```\n"),
    ("fenced indented", "  ```\n  code\n  ```\n"),
    ("fenced blank inside", "```\na\n\nb\n```\n"),
    ("fenced in list item", "- a\n\n  ```\n  x\n  ```\n"),
    ("fenced in blockquote", "> ```\n> x\n> ```\n"),
    (
        "fenced in nested list",
        "- a\n  1. b\n\n     ```\n     x\n     ```\n",
    ),
    (
        "fenced in list then item",
        "- a\n\n  ```\n  x\n  ```\n\n- b\n",
    ),
    ("heading in list item", "- a\n\n  # H\n"),
    ("heading in blockquote", "> # H\n>\n> text\n"),
    ("blockquote bare marker line", "> a\n>\n> b\n"),
    ("fenced longer close", "````\na\n```\nb\n````\n"),
    ("indented code", "    code\n"),
    ("indented code multiline", "    a\n    b\n"),
    ("thematic break", "---\n"),
    ("thematic break stars", "***\n"),
    ("thematic break indented", "   ***\n"),
    ("thematic break underscores", "_____\n"),
    ("blockquote", "> quoted\n> more\n"),
    ("blockquote indented", "  > q\n"),
    ("blockquote nested", "> a\n> > b\n"),
    ("blockquote list", "> - a\n> - b\n"),
    ("blockquote deep", "> > > > deep\n"),
    ("html block", "<div>\n  <p>x</p>\n</div>\n"),
    ("html block indented", "  <div>\n  x\n  </div>\n"),
    ("html comment", "<!-- comment -->\n"),
    ("table full pipes", "| a | b |\n|---|---|\n| 1 | 2 |\n"),
    ("table no outer pipes", "a | b\n---|---\n1 | 2\n"),
    ("table leading pipe only", "| a | b\n|---|---\n"),
    ("table ragged", "| a | b |\n|---|---|\n| 1 |\n"),
    ("table extra cells", "| a | b |\n|---|---|\n| 1 | 2 | 3 |\n"),
    (
        "table escaped pipe",
        "| a \\| b | c |\n|---|---|\n| 1 | 2 |\n",
    ),
    ("table empty cell", "| a |  |\n|---|---|\n| 1 | 2 |\n"),
    ("table indented", "  | a | b |\n  |---|---|\n  | 1 | 2 |\n"),
    ("table trailing ws", "| a | b |   \n|---|---|\n"),
    (
        "table alignment",
        "| a | b | c |\n|:--|:-:|--:|\n| 1 | 2 | 3 |\n",
    ),
    (
        "table padded delimiter",
        "| a | b |\n| --- | --- |\n| 1 | 2 |\n",
    ),
    (
        "table padded cells",
        "|  a  |  b  |\n|---|---|\n|  1  |  2  |\n",
    ),
    ("refdef alone", "[a]: /url\n"),
    ("refdef with title", "[a]: /url \"t\"\n"),
    ("refdefs then para", "[a]: /u\n[b]: /v\n\ntext\n"),
    ("refdef mixed prose", "[a]: /u\nbar\n"),
    ("refdef blank then para", "[a]: /u\n\ntext\n"),
    ("refdef in blockquote", "> [a]: /u\n"),
    ("crlf", "# H\r\n\r\ntext\r\n"),
    ("crlf list", "- a\r\n- b\r\n"),
    ("empty", ""),
    ("whitespace only", "   \n"),
    ("blank lines only", "\n\n\n"),
    ("no trailing newline", "text"),
    ("heading no trailing newline", "# H"),
    ("two blank lines", "a\n\n\nb\n"),
    ("trailing blank lines", "a\n\n\n"),
    ("unicode", "# 标题\n\n正文内容\n"),
    ("unicode table", "| 名前 | 値 |\n|---|---|\n| あ | い |\n"),
    ("math block", "$$\ny\n$$\n"),
    ("math inline", "text $x^2$ more\n"),
    // End-of-file without a trailing newline keeps trailing whitespace inside `inline`, which a
    // newline-terminated line does not. Both halves of that asymmetry need pinning.
    ("paragraph eof trailing ws", "hello   "),
    ("atx eof trailing ws", "#  Heading "),
    ("atx eof closed trailing ws", "# H1 #   "),
    ("atx closed trailing ws", "# H1 #   \n"),
    ("multiline paragraph eof ws", "hello  \nworld  "),
    ("setext no trailing newline", "Title\n==="),
    ("setext underline trailing ws", "T\n===   \n"),
    ("blockquote paragraph eof ws", "> q  "),
    ("list item paragraph eof ws", "- a  "),
];

/// Shortens a source for display, keeping both ends so a diff stays readable next to a node list.
fn truncate(source: &str) -> String {
    let flat: String = source.chars().flat_map(|c| c.escape_default()).collect();
    if flat.len() <= 160 {
        return flat;
    }
    format!(
        "{}…[{} bytes]…{}",
        &flat[..100],
        source.len(),
        &flat[flat.len() - 50..]
    )
}

/// Diffs the facade against tree-sitter-md over the hand-written edge cases.
#[test]
fn oracle_edge_cases() {
    let mut failures = Vec::new();
    let mut unparsed = Vec::new();
    for (label, source) in EDGE_CASES {
        if dump_tree_sitter(source).is_none() {
            unparsed.push(*label);
        }
        if let Some(diff) = compare(label, source) {
            failures.push(diff);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} edge cases differ ({} unparseable by tree-sitter):\n{}",
        failures.len(),
        EDGE_CASES.len(),
        unparsed.len(),
        failures.join("\n")
    );

    // An allow-list entry that names no edge case, or that no longer hides a difference, is stale —
    // either the case was renamed or the facade was fixed and the entry should be deleted.
    for &(known, _) in KNOWN_PARSER_DIFFS {
        let source = edge_case(known);
        assert!(
            dump_tree_sitter(source).is_some(),
            "{known}: tree-sitter no longer parses this at all, so it belongs in \
             KNOWN_TREE_SITTER_FAILURES instead"
        );
        assert!(
            compare_allowing(known, source, false).is_some(),
            "{known} is allow-listed but the trees now agree — delete the entry"
        );
    }
    for &(known, _) in KNOWN_TREE_SITTER_FAILURES {
        assert!(
            unparsed.contains(&known),
            "{known} is listed as unparseable by tree-sitter but it parsed — delete the entry"
        );
    }
    for &label in &unparsed {
        assert!(
            KNOWN_TREE_SITTER_FAILURES
                .iter()
                .any(|&(known, _)| known == label),
            "tree-sitter could not parse {label:?}; add it to KNOWN_TREE_SITTER_FAILURES with a reason"
        );
    }
}

fn edge_case(label: &str) -> &'static str {
    EDGE_CASES
        .iter()
        .find(|&&(name, _)| name == label)
        .unwrap_or_else(|| panic!("{label} is allow-listed but is not an edge case"))
        .1
}

/// Diffs the facade against tree-sitter-md over the checked-in markdownlint parity fixtures.
#[test]
fn oracle_parity_fixtures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-samples/parity");
    let sources = collect_markdown(&root);
    assert!(
        sources.len() > 300,
        "expected the migrated fixture corpus at {}, found {} files",
        root.display(),
        sources.len()
    );
    assert_no_diffs(&sources, "parity fixture");
}

/// Diffs the facade against tree-sitter-md over a deterministic sample of the vault snapshot, when
/// present. The snapshot lives outside the repo, so this is a no-op on a fresh checkout.
///
/// A small budget is allowed here rather than zero, for two reasons, each of which has to be visible
/// in the file for the difference to be tolerated:
///
/// - **Degenerate tables.** tree-sitter-md gives up on a table row whose cells are all empty: it ends
///   one `pipe_table` and starts another, where comrak follows GFM and keeps a single table. Because
///   the row count differs, everything after it shifts, so headings and paragraphs downstream show up
///   in the diff too.
/// - **HTML blocks.** tree-sitter-md runs an HTML block past the blank line that CommonMark says ends
///   it, sometimes to the end of the document. comrak stops at the blank line, which is correct.
///
/// A file that differs without matching one of those triggers fails the test, so a new kind of
/// divergence cannot hide inside the allowance. The budget must not grow.
#[test]
fn oracle_vault_sample() {
    const BUDGET: usize = 10;

    let corpus =
        std::env::var("QUICKMARK_ORACLE_CORPUS").unwrap_or_else(|_| "/tmp/vaultcmp/corpus".into());
    let root = Path::new(&corpus);
    if !root.is_dir() {
        eprintln!("oracle_vault_sample: {root:?} absent, skipping");
        return;
    }
    let mut sources = collect_markdown(root);
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    sources.truncate(500);

    let degenerate_table = regex::Regex::new(r"(?m)^\s*\|(?:\s*\|)+\s*$").unwrap();
    let html_block = regex::Regex::new(r"(?m)^\s*</?[A-Za-z]").unwrap();

    let mut differing = Vec::new();
    for (label, source) in &sources {
        let Some(kinds) = diff_kinds(source) else {
            continue;
        };
        assert!(
            degenerate_table.is_match(source) || html_block.is_match(source),
            "{label} differs with no degenerate table or HTML block to explain it: {kinds:?}\n{}",
            compare(label, source).unwrap_or_default()
        );
        differing.push((label.clone(), kinds));
    }

    assert!(
        differing.len() <= BUDGET,
        "{} of {} vault files differ, over the budget of {BUDGET}",
        differing.len(),
        sources.len()
    );
    for (label, kinds) in &differing {
        eprintln!("  allowed diff: {} -> {:?}", label, kinds);
    }
    eprintln!(
        "oracle_vault_sample: {} of {} files differ, all explained (budget {BUDGET})",
        differing.len(),
        sources.len()
    );
}

/// The node kinds that genuinely differ between the two trees, ignoring order.
///
/// A multiset difference rather than a positional one: when the parsers disagree about how many rows
/// a table has, every later node shifts index and a positional diff blames the whole document.
fn diff_kinds(source: &str) -> Option<Vec<String>> {
    let expected = dump_tree_sitter(source)?;
    let actual = dump_facade(source);
    if expected == actual {
        return None;
    }

    let mut counts: std::collections::HashMap<&Rec, isize> = std::collections::HashMap::new();
    for rec in &expected {
        *counts.entry(rec).or_default() += 1;
    }
    for rec in &actual {
        *counts.entry(rec).or_default() -= 1;
    }

    let mut kinds: Vec<String> = counts
        .into_iter()
        .filter(|&(_, delta)| delta != 0)
        .map(|(rec, _)| rec.kind.clone())
        .collect();
    kinds.sort();
    kinds.dedup();
    Some(kinds)
}

/// Zero-tolerance comparison, used for the corpora that have no tolerated differences at all.
fn assert_no_diffs(sources: &[(String, String)], noun: &str) {
    let mut failures = Vec::new();
    let mut unparsed = 0;
    for (label, source) in sources {
        if dump_tree_sitter(source).is_none() {
            unparsed += 1;
        }
        if let Some(diff) = compare(label, source) {
            failures.push(diff);
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} {noun}s differ ({} unparseable by tree-sitter):\n{}",
        failures.len(),
        sources.len(),
        unparsed,
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn collect_markdown(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path: PathBuf = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            {
                if let Ok(source) = fs::read_to_string(&path) {
                    out.push((path.display().to_string(), source));
                }
            }
        }
    }
    out
}
