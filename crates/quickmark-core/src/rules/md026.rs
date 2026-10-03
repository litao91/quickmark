use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::{Rule, RuleType};

// MD026-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
#[serde(default)]
pub struct MD026TrailingPunctuationTable {
    pub punctuation: String,
}

impl Default for MD026TrailingPunctuationTable {
    fn default() -> Self {
        Self::with_default_punctuation()
    }
}

impl MD026TrailingPunctuationTable {
    pub fn with_default_punctuation() -> Self {
        Self {
            // markdownlint's `allPunctuationNoQuestion`.
            punctuation: ".,;:!。，；：！".to_string(),
        }
    }
}

pub(crate) struct MD026Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD026Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    fn check(&mut self, node: Node) {
        let Some(text) = heading_text(node) else {
            return;
        };
        let punctuation = self
            .context
            .config
            .linters
            .settings
            .trailing_punctuation
            .punctuation
            .clone();
        // An empty set makes markdownlint's character class match nothing at all.
        if punctuation.is_empty() {
            return;
        }

        // An `inline` at end of file keeps its trailing whitespace, which micromark's heading text
        // never carries, so the text is trimmed again here.
        let from = text.start_byte();
        let heading = {
            let source = self.context.document_content.borrow();
            source[from..text.end_byte()]
                .trim_end_matches([' ', '\t'])
                .to_string()
        };
        let Some(run) = trailing_punctuation(&heading, &punctuation) else {
            return;
        };
        // An HTML entity and a GitHub emoji code both end in punctuation that belongs to them.
        if is_html_entity(&heading) || is_gemoji_code(&heading) {
            return;
        }
        let end = self.context.point_at(from + heading.len());

        self.violations.push(RuleViolation::new(
            &MD026,
            format!("{} [Punctuation: '{}']", MD026.description, &heading[run..]),
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte: 0,
                end_byte: 0,
                start_point: crate::ast::Point {
                    row: end.row,
                    column: end.column - (heading.len() - run),
                },
                end_point: end,
            }),
        ));
    }
}

/// The node holding a heading's text — micromark's `atxHeadingText` and `setextHeadingText`, which
/// the facade synthesizes as the `inline` under the heading, or under a setext heading's paragraph.
/// It stops before a closing `#` run and before trailing whitespace, spans every line of a setext
/// heading, and never runs over the container prefixes that follow the heading's own line — all of
/// which reading the heading's byte range would get wrong.
fn heading_text(node: Node) -> Option<Node> {
    (0..node.child_count()).find_map(|index| {
        let child = node.child(index)?;
        match child.kind() {
            "inline" => Some(child),
            "paragraph" => heading_text(child),
            _ => None,
        }
    })
}

/// Where markdownlint's `\s*[<punctuation>]+$` starts in `text`, or `None` when it does not match.
/// The run includes the whitespace before it, which is why `# Heading .` reports `' .'`.
fn trailing_punctuation(text: &str, punctuation: &str) -> Option<usize> {
    let mut start = text.len();
    let mut found = false;
    for (index, ch) in text.char_indices().rev() {
        if punctuation.contains(ch) {
            start = index;
            found = true;
        } else if found && ch.is_whitespace() {
            start = index;
        } else {
            break;
        }
    }
    found.then_some(start)
}

impl RuleLinter for MD026Linter {
    fn feed(&mut self, node: &Node) {
        if matches!(node.kind(), "atx_heading" | "setext_heading") {
            self.check(*node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

/// markdownlint's `endOfLineHtmlEntityRe`. The named forms are a closed list, so `&a1;` and `&X41;`
/// are punctuation-terminated text rather than entities.
fn is_html_entity(text: &str) -> bool {
    static HTML_ENTITY_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"&(?:#\d+|#[xX][\da-fA-F]+|[a-zA-Z]{2,31}|blk\d{2}|emsp1[34]|frac\d{2}|sup\d|there4);$")
            .expect("Invalid HTML entity regex")
    });
    HTML_ENTITY_RE.is_match(text)
}

/// markdownlint's `endOfLineGemojiCodeRe`.
fn is_gemoji_code(text: &str) -> bool {
    static GEMOJI_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r":(?:[abmovx]|[-+]1|100|1234|(?:1st|2nd|3rd)_place_medal|8ball|clock\d{1,4}|e-mail|non-potable_water|o2|t-rex|u5272|u5408|u55b6|u6307|u6708|u6709|u6e80|u7121|u7533|u7981|u7a7a|[a-z]{2,15}2?|[a-z]{1,14}(?:_[a-z\d]{1,16})+):$")
            .expect("Invalid gemoji regex")
    });
    GEMOJI_RE.is_match(text)
}

pub const MD026: Rule = Rule {
    id: "MD026",
    aliases: &["no-trailing-punctuation"],
    tags: &["headings"],
    description: "Trailing punctuation in heading",
    rule_type: RuleType::Token,
    required_nodes: &["atx_heading", "setext_heading"],
    new_linter: |context| Box::new(MD026Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD026TrailingPunctuationTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    fn config(punctuation: &str) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("no-trailing-punctuation", RuleSeverity::Error)],
            LintersSettingsTable {
                trailing_punctuation: MD026TrailingPunctuationTable {
                    punctuation: punctuation.to_string(),
                },
                ..Default::default()
            },
        )
    }

    /// The 1-based line MD026 reports on and the punctuation run it names.
    fn reports(punctuation: &str, source: &str) -> Vec<(usize, String)> {
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            config(punctuation),
            source,
        );
        linter
            .analyze()
            .iter()
            .filter(|violation| violation.rule().id == "MD026")
            .map(|violation| {
                let run = violation
                    .message()
                    .split_once("[Punctuation: '")
                    .and_then(|(_, rest)| rest.strip_suffix("']"))
                    .unwrap_or_default()
                    .to_string();
                (violation.location().range.start.line + 1, run)
            })
            .collect()
    }

    fn with_default(source: &str) -> Vec<(usize, String)> {
        reports(".,;:!。，；：！", source)
    }

    /// A case's name, its document, and the line and punctuation run markdownlint reports.
    type Case = (&'static str, &'static str, &'static [(usize, &'static str)]);

    /// Every expectation measured against markdownlint-cli2 v0.23.3.
    #[test]
    fn matches_markdownlint() {
        let cases: &[Case] = &[
            ("period", "# Heading.\n", &[(1, ".")]),
            ("exclamation", "# Heading!\n", &[(1, "!")]),
            ("two exclamations", "# Heading!!\n", &[(1, "!!")]),
            ("two semicolons", "# Heading;;\n", &[(1, ";;")]),
            ("period then semicolon", "# Heading.;\n", &[(1, ".;")]),
            // The run is markdownlint's `\s*[...]+$`, whitespace included.
            (
                "space then comma and period",
                "# Heading ,.\n",
                &[(1, " ,.")],
            ),
            ("two spaces then period", "# Heading  .\n", &[(1, "  .")]),
            ("space then period", "# Heading .\n", &[(1, " .")]),
            ("space then full stop", "# Heading 。\n", &[(1, " 。")]),
            ("question mark is not in the set", "# Heading?\n", &[]),
            ("ends in a question mark", "# Heading!?\n", &[]),
            ("punctuation mid-text", "# Heading...more\n", &[]),
            ("punctuation inside a word", "# a.b.\n", &[(1, ".")]),
            ("four hashes", "#### Heading.\n", &[(1, ".")]),
            ("punctuation alone", "# .\n", &[(1, ".")]),
            ("trailing spaces after it", "# Heading.  \n", &[(1, ".")]),
            ("a tab after it", "# Heading.\t\n", &[(1, ".")]),
            // An `inline` at end of file keeps trailing whitespace, which heading text never has.
            ("no final newline", "##  Heading! ", &[(1, "!")]),
            ("a closing hash run", "# Heading #\n", &[]),
            (
                "punctuation before a closing run",
                "# Heading. #\n",
                &[(1, ".")],
            ),
            ("empty heading", "#\n", &[]),
            ("heading of only spaces", "## \n", &[]),
            ("full-width semicolon", "# Heading；\n", &[(1, "；")]),
            ("two full-width stops", "# 。。\n", &[(1, "。。")]),
            ("cjk text", "# 标题！\n", &[(1, "！")]),
            ("full-width then half-width", "# a！b？\n", &[]),
            ("html entity", "# &amp;\n", &[]),
            ("uppercase entity name", "# &AMP;\n", &[]),
            ("numeric entity", "# &#33;\n", &[]),
            ("a named form from the list", "# &there4;\n", &[]),
            ("another named form", "# &emsp13;\n", &[]),
            // The named forms are a closed list, so these are text ending in a semicolon.
            ("digits in an entity name", "# &a1;\n", &[(1, ";")]),
            ("a hex entity without the hash", "# &X41;\n", &[(1, ";")]),
            ("a bare ampersand", "# &\n", &[]),
            ("gemoji code", "# :smile:\n", &[]),
            ("gemoji with digits", "# :+1:\n", &[]),
            ("gemoji with a clock", "# :clock1030:\n", &[]),
            (
                "a long snake-case code",
                "# :not_a_gemoji_code_here_though:\n",
                &[],
            ),
            ("setext with punctuation", "Setext!\n=======\n", &[(1, "!")]),
            (
                "setext with a dash underline",
                "Setext.\n---\n",
                &[(1, ".")],
            ),
            ("setext without punctuation", "Setext\n======\n", &[]),
            (
                "setext with an indented underline",
                "Setext\n  ======\n",
                &[],
            ),
            // The text of a setext heading is every line of it, not just the first.
            ("multi-line setext", "a\nb.\n===\n", &[(2, ".")]),
            ("after a paragraph", "text\n\n# Heading.\n", &[(3, ".")]),
            ("in a list item", "- # Heading.\n", &[(1, ".")]),
            (
                "in a list item's continuation",
                "- item\n\n  # Heading.\n",
                &[(3, ".")],
            ),
            (
                "setext in a list item",
                "- item\n\n  Setext.\n  -------\n",
                &[(3, ".")],
            ),
            // The shape the vault comparison turned up: reading the heading's own byte range runs
            // on over the `>` prefixes that follow it, so the text ends in `>` and nothing matches.
            (
                "in a block quote that continues",
                "> # Heading.\n>\n> more\n",
                &[(1, ".")],
            ),
            (
                "in a block quote",
                "> # Important Discovery!\n",
                &[(1, "!")],
            ),
            (
                "in a block quote after text",
                "text\n\n> # Discovery!\n",
                &[(3, "!")],
            ),
            ("in a table cell", "| a |\n| - |\n| # H. |\n", &[]),
        ];

        let failures: Vec<String> = cases
            .iter()
            .filter(|&&(name, source, expected)| {
                let actual = with_default(source);
                let expected: Vec<(usize, String)> = expected
                    .iter()
                    .map(|&(line, run)| (line, run.to_string()))
                    .collect();
                if actual == expected {
                    return false;
                }
                println!("{name}: expected {expected:?}, got {actual:?}");
                true
            })
            .map(|&(name, _, _)| name.to_string())
            .collect();
        assert!(
            failures.is_empty(),
            "{} of {} cases disagree with markdownlint: {failures:?}",
            failures.len(),
            cases.len()
        );
    }

    /// Measured against markdownlint with `"punctuation": ".,;:"`.
    #[test]
    fn a_configured_set_replaces_the_default() {
        assert_eq!(
            with_config(".,;:", "# This heading has exclamation!"),
            vec![]
        );
        assert_eq!(
            with_config(".,;:", "# This heading has period."),
            vec![(1, ".".to_string())]
        );
        assert_eq!(with_config(".,;:", "# This has a comma, and bang!"), vec![]);
        // An empty set leaves markdownlint's character class matching nothing.
        assert_eq!(
            with_config("", "# Heading.\n## Heading!\n### Heading,"),
            vec![]
        );
    }

    fn with_config(punctuation: &str, source: &str) -> Vec<(usize, String)> {
        reports(punctuation, source)
    }

    #[test]
    fn a_report_covers_the_punctuation() {
        let mut linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            config(".,;:!。，；：！"),
            "> # Heading.\n>\n> more\n",
        );
        let range = linter
            .analyze()
            .iter()
            .find(|violation| violation.rule().id == "MD026")
            .map(|violation| {
                let range = &violation.location().range;
                (
                    range.start.line,
                    range.start.character,
                    range.end.line,
                    range.end.character,
                )
            })
            .expect("one violation");
        // markdownlint reports `[endColumn - length, length]` on the text's last line.
        assert_eq!(range, (0, 11, 0, 12));
    }
}
