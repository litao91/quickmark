use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::{
    linter::{CharPosition, Range, RuleViolation},
    rules::{
        md052::{bracket_label, normalize_label},
        Context, Rule, RuleLinter, RuleType,
    },
};

// MD053-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD053LinkImageReferenceDefinitionsTable {
    // Not `#[serde(default)]`: a settings table that sets nothing still gets the default ignored
    // definitions, which is what markdownlint's `config.ignored_definitions || ["//"]` does.
    #[serde(default = "default_ignored_definitions")]
    pub ignored_definitions: Vec<String>,
}

fn default_ignored_definitions() -> Vec<String> {
    vec!["//".to_string()]
}

impl Default for MD053LinkImageReferenceDefinitionsTable {
    fn default() -> Self {
        Self {
            ignored_definitions: default_ignored_definitions(),
        }
    }
}

/// The label on a definition's first line. The node's shape is the parser's business — this only cuts
/// the label out, and a label may hold escapes, so `\]` does not end it.
static DEFINITION_LABEL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[ \t]{0,3}\[((?:[^\]\\]|\\.)*)\]:").expect("Invalid definition label pattern")
});

#[derive(Debug, Clone)]
struct Definition {
    label: String,
    row: usize,
}

pub(crate) struct MD053Linter {
    context: Rc<Context>,
    /// The first definition of each label, in document order.
    definitions: Vec<Definition>,
    /// Every definition after the first of its label, in document order.
    duplicates: Vec<Definition>,
    seen: HashSet<String>,
    used: HashSet<String>,
}

impl MD053Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            definitions: Vec::new(),
            duplicates: Vec::new(),
            seen: HashSet::new(),
            used: HashSet::new(),
        }
    }

    /// Records one definition, keeping the first of each label and listing the rest as duplicates —
    /// markdownlint splits them the same way, and a later definition of a label is unreachable
    /// whichever way round the two are reported.
    fn add_definition(&mut self, node: Node) {
        let definition = {
            let source = self.context.document_content.borrow();
            let text = &source[node.start_byte()..node.end_byte()];
            // A definition runs on to the lines carrying its title, and its label is on the first.
            let first_line = text.split(['\n', '\r']).next().unwrap_or(text);
            DEFINITION_LABEL.captures(first_line).and_then(|found| {
                found.get(1).map(|label| Definition {
                    label: normalize_label(label.as_str()),
                    row: node.start_position().row,
                })
            })
        };
        let Some(definition) = definition else {
            return;
        };
        if self.seen.insert(definition.label.clone()) {
            self.definitions.push(definition);
        } else {
            self.duplicates.push(definition);
        }
    }

    /// The labels one inline subtree refers to.
    ///
    /// Only what the parser resolved counts, and a resolved reference is a `link` or `image` node
    /// whose own text still holds the label. Syntax that resolved to nothing names no definition, so
    /// it has nothing to keep from being reported unused.
    fn collect_uses(&mut self, root: Node) {
        let mut found = Vec::new();
        {
            let source = self.context.document_content.borrow();
            let mut cursor = root.walk();
            let mut depth = 0;
            loop {
                let node = cursor.node();
                if matches!(node.kind(), "link" | "image") {
                    if let Ok(text) = node.utf8_text(source.as_bytes()) {
                        // An inline link ends in `)` and an autolink in `>`; only reference syntax
                        // names a label, and it ends in the `]` that closed it.
                        if text.ends_with(']') {
                            found.extend(
                                bracket_label(text)
                                    .map(|(from, to)| normalize_label(&text[from..to])),
                            );
                        }
                    }
                }

                if cursor.goto_first_child() {
                    depth += 1;
                    continue;
                }
                loop {
                    if depth == 0 {
                        self.used.extend(found);
                        return;
                    }
                    if cursor.goto_next_sibling() {
                        break;
                    }
                    cursor.goto_parent();
                    depth -= 1;
                }
            }
        }
    }

    /// markdownlint's `errorRange` for a definition is its whole first line, however many lines the
    /// definition itself spans.
    fn report(&self, kind: &str, definition: &Definition) -> RuleViolation {
        let width = self
            .context
            .lines
            .borrow()
            .get(definition.row)
            .map_or(0, |line| line.len());
        RuleViolation::new(
            &MD053,
            format!(
                "{kind} link or image reference definition: \"{}\"",
                definition.label
            ),
            self.context.file_path.clone(),
            Range {
                start: CharPosition {
                    line: definition.row,
                    character: 0,
                },
                end: CharPosition {
                    line: definition.row,
                    character: width,
                },
            },
        )
    }
}

impl RuleLinter for MD053Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            "link_reference_definition" => self.add_definition(*node),
            "inline" => self.collect_uses(*node),
            _ => {}
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        let config = &self
            .context
            .config
            .linters
            .settings
            .link_image_reference_definitions;
        let ignored: HashSet<String> = config
            .ignored_definitions
            .iter()
            .map(|label| normalize_label(label))
            .collect();

        let mut violations = Vec::new();
        // markdownlint walks its unused definitions and then its duplicates, so every "Unused" comes
        // before every "Duplicate" whatever the document order.
        for definition in std::mem::take(&mut self.definitions) {
            if !ignored.contains(&definition.label) && !self.used.contains(&definition.label) {
                violations.push(self.report("Unused", &definition));
            }
        }
        for definition in std::mem::take(&mut self.duplicates) {
            if !ignored.contains(&definition.label) {
                violations.push(self.report("Duplicate", &definition));
            }
        }
        violations
    }
}

pub const MD053: Rule = Rule {
    id: "MD053",
    alias: "link-image-reference-definitions",
    tags: &["links", "images"],
    description: "Link and image reference definitions should be needed",
    rule_type: RuleType::Document,
    required_nodes: &["inline", "link_reference_definition"],
    new_linter: |context| Box::new(MD053Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{
        LintersSettingsTable, MD053LinkImageReferenceDefinitionsTable, RuleSeverity,
    };
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    /// One report: markdownlint's line, the kind it names, and the label. Its `errorRange` is always
    /// the definition's whole line, so there is no column to assert.
    type Report = (usize, &'static str, &'static str);

    /// The same report with owned strings, which is what splitting a message can hand back.
    type Found = (usize, String, String);

    fn owned(expected: &[Report]) -> Vec<Found> {
        expected
            .iter()
            .map(|&(line, kind, label)| (line, kind.to_string(), label.to_string()))
            .collect()
    }

    fn config_with(
        table: MD053LinkImageReferenceDefinitionsTable,
    ) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("link-image-reference-definitions", RuleSeverity::Error)],
            LintersSettingsTable {
                link_image_reference_definitions: table,
                ..Default::default()
            },
        )
    }

    fn reports_with(config: crate::config::QuickmarkConfig, source: &str) -> Vec<Found> {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, source);
        let violations = linter.analyze();
        violations
            .iter()
            .map(|violation| {
                let message = violation.message();
                let (kind, label) = message
                    .split_once(" link or image reference definition: \"")
                    .and_then(|(kind, rest)| rest.strip_suffix('"').map(|label| (kind, label)))
                    .unwrap_or((message, ""));
                (
                    violation.location().range.start.line + 1,
                    kind.to_string(),
                    label.to_string(),
                )
            })
            .collect()
    }

    fn reports(source: &str) -> Vec<Found> {
        reports_with(
            config_with(MD053LinkImageReferenceDefinitionsTable::default()),
            source,
        )
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output with
    /// MD053's defaults.
    const CASES: &[(&str, &[Report])] = &[
        ("[a]: /u\n\ntext\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n\n[x][a]\n", &[]),
        ("[a]: /u\n\n[a]\n", &[]),
        ("[a]: /u\n\n[a][]\n", &[]),
        ("[a]: /u\n\n![x][a]\n", &[]),
        ("[a]: /u\n[a]: /v\n\n[x][a]\n", &[(2, "Duplicate", "a")]),
        (
            "[a]: /u\n[a]: /v\n\ntext\n",
            &[(1, "Unused", "a"), (2, "Duplicate", "a")],
        ),
        ("[a]: /u\n[b]: /v\n\n[x][a]\n", &[(2, "Unused", "b")]),
        ("[//]: # (c)\n[a]: /u\n\ntext\n", &[(2, "Unused", "a")]),
        ("[a]: /u\n  \"title\"\n\ntext\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n\n[x][a] and (y)\n", &[]),
        ("[a]: /u\n\n`[x][a]`\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n\n$x[a]$\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n\n<div>[x][a]</div>\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n\n# [x][a]\n", &[]),
        ("[a]: /u\n\n> [x][a]\n", &[]),
        ("[a]: /u\n\n- [x][a]\n", &[]),
        ("[a]: /u\n\n| h |\n|---|\n| [x][a] |\n", &[]),
        ("[a]: /u\n\n[x\n y][a]\n", &[]),
        ("[a]: /u\n\n[x][A]\n", &[]),
        ("[  A  b ]: /u\n\n[x][a b]\n", &[]),
        ("[t]: /u\n\n[t](http://x)\n", &[(1, "Unused", "t")]),
        ("[a]: /u\n[b]: /v\n\n[a][b]\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n[b]: /v\n\n[b][a]\n", &[(2, "Unused", "b")]),
        ("[a]: /u\n\n![x](http://y)\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n\n[a](http://x) [a]\n", &[]),
        ("[a]: /u\n\n[A]\n", &[]),
        ("[a]: /u\n\n[x][a] [a][]\n", &[]),
        ("[a]: /u\n\n---\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n[b]: /v\n\n[a][b] [b][a]\n", &[]),
        ("[a]: /u\n\n[outer [a] text](http://x)\n", &[]),
        ("[a]: /u\n\n[![i][a]][b]\n", &[]),
        ("[a]: /u\n\n[outer [a] text]\n", &[]),
        ("[a]: /u\n\n**[a]**\n", &[]),
        ("[a]: /u\n\n<a href='[a]'>x</a>\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n\n[x][a]\n[a]: /v\n", &[]),
        ("[a]: /u\n\n\\[a]\n", &[(1, "Unused", "a")]),
        ("[a]: some prose here\n\ntext\n", &[]),
        ("[a]: /u \"t\" junk\n\ntext\n", &[]),
        ("[a]: /u\n\n[a][b][c]\n", &[(1, "Unused", "a")]),
        ("[^a]: /u\n\ntext\n", &[(1, "Unused", "^a")]),
        ("[a]: /u\n\n> [x][a]\n\n[a]: /v\n", &[(5, "Duplicate", "a")]),
        ("[a]: /u\n\n[x][a]\r\n", &[]),
        ("[a]: <>\n\ntext\n", &[(1, "Unused", "a")]),
        ("[a]: /u\n\n[the `x` trait][a]\n", &[]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            assert_eq!(owned(expected), reports(source), "source {source:?}");
        }
    }

    /// Three shapes markdownlint reports and quickmark does not.
    ///
    /// The first two are the facade's, not this rule's: comrak detaches a link reference definition
    /// and leaves no trace of it, so the tree is rebuilt from the lines nothing else claimed, and a
    /// definition whose line a list item already covers, or whose destination sits on the line after
    /// its label, is not among them. The third is a missing parser extension — markdownlint turns on
    /// micromark's GFM footnotes, so `[^a]: prose` is a footnote definition whose label is `^a`,
    /// while here it is an ordinary paragraph. `[^a]: /u` is reported either way, because that one is
    /// also a valid link reference definition.
    #[test]
    fn known_differences_from_markdownlint() {
        let none: &[Report] = &[];
        // markdownlint: [(1, "Unused", "a")]
        assert_eq!(owned(none), reports("- [a]: /u\n\ntext\n"));
        // markdownlint: [(1, "Unused", "a")]
        assert_eq!(owned(none), reports("[a]:\n/u\n\ntext\n"));
        // markdownlint: [(1, "Unused", "^a")]
        assert_eq!(owned(none), reports("[^a]: prose here\n\ntext\n"));
    }

    #[test]
    fn ignored_definitions_replace_the_default() {
        let source = "[//]: # (c)\n[a]: /u\n\ntext\n";
        let config = config_with(MD053LinkImageReferenceDefinitionsTable {
            ignored_definitions: vec!["a".to_string()],
        });
        // markdownlint reports only `//`: setting `ignored_definitions` drops the default `//`.
        assert_eq!(owned(&[(1, "Unused", "//")]), reports_with(config, source));
        assert_eq!(owned(&[(2, "Unused", "a")]), reports(source));
    }
}
