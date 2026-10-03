use serde::Deserialize;
use std::rc::Rc;

use crate::ast::Node;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::{
    linter::{range_from_node_range, Context, RuleLinter, RuleViolation},
    rules::{ellipsify, Rule, RuleType},
};

// MD041-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
#[serde(default)]
pub struct MD041FirstLineHeadingTable {
    pub allow_preamble: bool,
    pub front_matter_title: String,
    pub level: u8,
}

impl Default for MD041FirstLineHeadingTable {
    fn default() -> Self {
        Self {
            allow_preamble: false,
            front_matter_title: r"^\s*title\s*[:=]".to_string(),
            level: 1,
        }
    }
}

#[derive(Debug)]
enum FirstElement {
    Heading(u8, crate::ast::NodeRange), // level, range
    Content(crate::ast::NodeRange),
    None,
}

/// The tag an HTML block opens with. markdownlint's `getHtmlTagInfo`, which is CommonMark's tag-name
/// grammar: neither `!` nor `>` may start one, and it ends at a slash, a space, a `>` or the end.
static HTML_TAG_NAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^<([^!>][^/\s>]*)").expect("Invalid HTML tag pattern"));

/// markdownlint's `frontMatterRe`, ported character for character: three delimiter pairs, a `---`
/// closed by `---`, a `+++` closed by `+++` or `...`, and a `{` closed by `}`.
///
/// Front matter is this pattern and not the parser's opinion of it, because markdownlint cuts the
/// match out of the content before it parses and only accepts one anchored at byte zero. The parser
/// here is narrower — it wants content between the delimiters, and knows nothing of `{` — so a
/// document the pattern matches but the parser did not strip still has to be treated as starting
/// after it, and the other way round.
static FRONT_MATTER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?m)((^---[^\S\r\n\u{2028}\u{2029}]*\r?$[\s\S]+?^---\s*)|(^\+\+\+[^\S\r\n\u{2028}\u{2029}]*\r?$[\s\S]+?^(\+\+\+|\.\.\.)\s*)|(^\{[^\S\r\n\u{2028}\u{2029}]*\r?$[\s\S]+?^\}\s*))(\r\n|\r|\n|$)"
    )
    .expect("Invalid front matter pattern")
});

pub(crate) struct MD041Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    first_element: FirstElement,
    /// Byte just past the front matter, if the document opens with any.
    front_matter_end: Option<usize>,
    title_regex: Option<Regex>,
}

impl MD041Linter {
    pub fn new(context: Rc<Context>) -> Self {
        let config = &context.config.linters.settings.first_line_heading;
        let title_regex = if !config.front_matter_title.is_empty() {
            Some(
                Regex::new(&config.front_matter_title)
                    .unwrap_or_else(|_| Regex::new(r"^\s*title\s*[:=]").unwrap()),
            )
        } else {
            None
        };
        let front_matter_end = {
            let content = context.get_document_content();
            // The match has to start at byte zero, and only a document opening with a delimiter can
            // have one, so the rest are not scanned at all.
            let opens = content.starts_with(['-', '+', '{']);
            opens
                .then(|| FRONT_MATTER.find(&content))
                .flatten()
                .filter(|found| found.start() == 0)
                .map(|found| found.end())
        };

        Self {
            context: context.clone(),
            violations: Vec::new(),
            first_element: FirstElement::None,
            front_matter_end,
            title_regex,
        }
    }

    fn extract_heading_level(&self, node: &Node) -> u8 {
        match node.kind() {
            "atx_heading" => {
                for i in 0..node.child_count() {
                    let child = node.child(i).unwrap();
                    let kind = child.kind();
                    if kind.starts_with("atx_h") && kind.ends_with("_marker") {
                        let level_str = &kind["atx_h".len()..kind.len() - "_marker".len()];
                        return level_str.parse::<u8>().unwrap_or(1);
                    }
                }
                1 // fallback
            }
            "setext_heading" => {
                for i in 0..node.child_count() {
                    let child = node.child(i).unwrap();
                    if child.kind() == "setext_h1_underline" {
                        return 1;
                    } else if child.kind() == "setext_h2_underline" {
                        return 2;
                    }
                }
                1 // fallback
            }
            _ => 1,
        }
    }

    /// Whether the front matter names a title, which markdownlint accepts in place of a heading.
    fn check_front_matter_has_title(&self) -> bool {
        let Some(title_regex) = &self.title_regex else {
            return false; // Front matter title checking disabled
        };
        let Some(end) = self.front_matter_end else {
            return false; // No front matter found
        };

        let content = self.context.get_document_content();
        content[..end]
            .lines()
            .skip(1) // Skip the opening delimiter
            .any(|line| title_regex.is_match(line))
    }

    /// The level of an HTML block that opens with an `<h1>`-`<h6>` tag, which markdownlint accepts in
    /// place of a heading. `<head>` is not one, and neither is `</h1>`'s counterpart `<h1x>`.
    fn html_heading_level(&self, node: &Node) -> Option<u8> {
        let source = self.context.get_document_content();
        let text = &source[node.start_byte()..node.end_byte()];
        let name = HTML_TAG_NAME.captures(text)?.get(1)?.as_str();
        // markdownlint tests the lowercased name against `/^h[1-6]$/`, and its tag info strips a
        // closing tag's slash first, so `</h2>` counts as a level-two heading.
        let name = name.strip_prefix('/').unwrap_or(name).to_lowercase();
        let level = match name.as_str() {
            "h1" => 1,
            "h2" => 2,
            "h3" => 3,
            "h4" => 4,
            "h5" => 5,
            "h6" => 6,
            _ => return None,
        };
        Some(level)
    }

    /// Whether an HTML block is nothing but a comment, which markdownlint steps over.
    ///
    /// The three extra conditions are HTML's, not CommonMark's: `<!-->`, `<!--->` and `<!-- x --->`
    /// are all blocks that merely start and end like a comment.
    fn is_html_comment(&self, node: &Node) -> bool {
        if node.kind() != "html_block" {
            return false;
        }
        let source = self.context.get_document_content();
        let text = source[node.start_byte()..node.end_byte()].trim_end();
        let Some(comment) = text
            .strip_prefix("<!--")
            .and_then(|t| t.strip_suffix("-->"))
        else {
            return false;
        };
        !comment.starts_with('>') && !comment.starts_with("->") && !comment.ends_with('-')
    }
    /// markdownlint has one message for both halves of this rule, quoting the line it reports on.
    fn violation(&self, range: &crate::ast::NodeRange) -> RuleViolation {
        let line = self
            .context
            .lines
            .borrow()
            .get(range.start_point.row)
            .cloned()
            .unwrap_or_default();
        RuleViolation::new(
            &MD041,
            format!(
                "{} [Context: \"{}\"]",
                MD041.description,
                ellipsify(&line, false, false)
            ),
            self.context.file_path.clone(),
            range_from_node_range(range),
        )
    }
}

/// Whether a node is one of the document's own blocks rather than something nested in one.
///
/// markdownlint walks the top-level token list; the facade groups those tokens into `section` nodes,
/// which have no counterpart there, so a top-level block is a child of `document` or of a `section`
/// and is never a `section` itself.
fn is_top_level(node: &Node) -> bool {
    !matches!(node.kind(), "document" | "section")
        && node
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), "document" | "section"))
}

impl RuleLinter for MD041Linter {
    fn feed(&mut self, node: &Node) {
        // Only the first element matters, and only the document's own blocks are elements. Front
        // matter is not one: markdownlint cuts it out of the content before parsing.
        if !matches!(self.first_element, FirstElement::None) || !is_top_level(node) {
            return;
        }
        if self
            .front_matter_end
            .is_some_and(|end| node.start_byte() < end)
        {
            return;
        }
        // A comment is not content either, so the element after it is still the first.
        if self.is_html_comment(node) {
            return;
        }

        self.first_element = match node.kind() {
            "atx_heading" | "setext_heading" => {
                FirstElement::Heading(self.extract_heading_level(node), node.range())
            }
            "html_block" => match self.html_heading_level(node) {
                Some(level) => FirstElement::Heading(level, node.range()),
                None => FirstElement::Content(node.range()),
            },
            _ => FirstElement::Content(node.range()),
        };
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        // Check if front matter has title - if so, no violation
        if self.check_front_matter_has_title() {
            return Vec::new();
        }

        let config = &self.context.config.linters.settings.first_line_heading;

        match &self.first_element {
            FirstElement::Heading(level, range) => {
                // First element is a heading - check if it has the correct level
                if *level != config.level {
                    self.violations.push(self.violation(range));
                }
            }
            FirstElement::Content(range) => {
                // First element is content - only a violation if preamble is not allowed
                if !config.allow_preamble {
                    self.violations.push(self.violation(range));
                }
            }
            FirstElement::None => {
                // No content found - this is valid (empty document)
            }
        }

        std::mem::take(&mut self.violations)
    }
}

pub const MD041: Rule = Rule {
    id: "MD041",
    alias: "first-line-heading",
    tags: &["headings"],
    description: "First line in a file should be a top-level heading",
    rule_type: RuleType::Document,
    // Every kind can be the document's first element, so there is nothing to enumerate.
    required_nodes: &[],
    new_linter: |context| Box::new(MD041Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD041FirstLineHeadingTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    fn test_config(
        level: u8,
        front_matter_title: &str,
        allow_preamble: bool,
    ) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("first-line-heading", RuleSeverity::Error)],
            LintersSettingsTable {
                first_line_heading: MD041FirstLineHeadingTable {
                    level,
                    front_matter_title: front_matter_title.to_string(),
                    allow_preamble,
                },
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_valid_first_line_heading() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "# Title

Some content

## Section 1

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_no_first_line_heading() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "This is some text

# Title

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
        assert!(violations[0]
            .message()
            .contains("First line in a file should be a top-level heading"));
    }

    #[test]
    fn test_wrong_level_first_heading() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "## Title

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
        assert!(violations[0].message().contains(
            "First line in a file should be a top-level heading [Context: \"## Title\"]",
        ));
    }

    #[test]
    fn test_custom_level() {
        let config = test_config(2, r"^\s*title\s*[:=]", false);
        let input = "## Title

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_custom_level_wrong_level() {
        let config = test_config(2, r"^\s*title\s*[:=]", false);
        let input = "# Title

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
        assert!(
            violations[0].message().contains(
                "First line in a file should be a top-level heading [Context: \"# Title\"]",
            )
        );
    }

    #[test]
    fn test_setext_heading_valid() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "Title
=====

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_setext_heading_wrong_level() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "Title
-----

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
        assert!(violations[0]
            .message()
            .contains("First line in a file should be a top-level heading [Context: \"Title\"]",));
    }

    #[test]
    fn test_allow_preamble_true() {
        let config = test_config(1, r"^\s*title\s*[:=]", true);
        let input = "This is some preamble text

# Title

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_allow_preamble_false() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "This is some preamble text

# Title

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
        assert!(violations[0]
            .message()
            .contains("First line in a file should be a top-level heading"));
    }

    #[test]
    fn test_front_matter_with_title() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "---
layout: post
title: \"Welcome to Jekyll!\"
date: 2015-11-17 16:16:01 -0600
---

This is content without a heading";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_front_matter_without_title() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "---
layout: post
author: John Doe
date: 2015-11-17 16:16:01 -0600
---

This is content without a heading";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn test_front_matter_title_disabled() {
        let config = test_config(1, "", false); // Empty pattern disables front matter checking
        let input = "---
title: \"Welcome to Jekyll!\"
---

This is content without a heading";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn test_custom_front_matter_title_regex() {
        let config = test_config(1, r"^\s*heading\s*:", false);
        let input = "---
layout: post
heading: \"My Custom Title\"
---

This is content without a heading";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_comments_before_heading() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "<!-- This is a comment -->

# Title

Content";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_empty_document() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    #[test]
    fn test_whitespace_only() {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let input = "   \n\n  \n\n# Title\n\nContent";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(violations.len(), 0);
    }

    /// The line markdownlint reports, 1-based, or nothing when the document is acceptable.
    type Line = usize;

    fn lines(source: &str) -> Vec<Line> {
        let config = test_config(1, r"^\s*title\s*[:=]", false);
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, source);
        linter
            .analyze()
            .iter()
            .map(|violation| violation.location().range.start.line + 1)
            .collect()
    }

    /// Every expectation below is markdownlint-cli2 v0.23.3 (markdownlint v0.41.1) output with
    /// MD041's defaults.
    const CASES: &[(&str, &[Line])] = &[
        // The first element decides, whatever kind it is.
        ("# Heading\n", &[]),
        ("## Heading\n", &[1]),
        ("text\n", &[1]),
        ("[a]: /u\n\n# H\n", &[1]),
        ("> quote\n", &[1]),
        ("- item\n", &[1]),
        ("```\ncode\n```\n", &[1]),
        ("    indented\n", &[1]),
        ("---\n", &[1]),
        ("| a |\n|---|\n", &[1]),
        ("$$\nmath\n$$\n", &[1]),
        ("Setext\n======\n", &[]),
        ("Setext\n---\n", &[1]),
        ("   # H\n", &[]),
        ("\n\n# H\n", &[]),
        ("", &[]),
        // A heading level that is not a heading at all.
        ("#\u{fe0f}\u{20e3} H\n", &[1]),
        // An HTML block stands in for a heading only when it opens with the expected `<h?>` tag,
        // and a closing tag counts because markdownlint's tag info strips the slash.
        ("<h1>x</h1>\n", &[]),
        ("<h1>x</h1>\ntext\n", &[]),
        ("<h1>x</h1>\n\nmore\n", &[]),
        ("<H1>x</H1>\n", &[]),
        ("</h1>\ntext\n", &[]),
        ("<h2>x</h2>\n", &[1]),
        ("<h3>x</h3>\n", &[1]),
        ("<h7>x</h7>\n", &[1]),
        ("<h1x>y</h1x>\n", &[1]),
        (
            "<head>\n  <front>matter</front>\n</head>\n# Heading\n",
            &[1],
        ),
        ("<div>\n# not a heading\n</div>\n", &[1]),
        // A comment is stepped over, but only one that HTML also calls a comment.
        ("<!-- comment -->\n# H\n", &[]),
        ("<!-- c -->\n<!-- d -->\n# H\n", &[]),
        ("<!-- a --> <!-- b -->\n# H\n", &[]),
        ("<!-- c -->\n<div>x</div>\n", &[2]),
        ("<!-- comment -->\n\ntext\n", &[3]),
        ("<!-- c -->", &[]),
        ("# H\n<!-- c -->\n", &[]),
        ("<!--> x -->\n# H\n", &[1]),
        ("<!---> x -->\n# H\n", &[1]),
        ("<!-- x --->\n# H\n", &[1]),
        // All three of markdownlint's front matter delimiter pairs, either line ending, and a
        // `title` that stands in for the heading.
        ("---\nfm\n---\n# H\n", &[]),
        ("---\nfm\n---", &[]),
        ("---\nfm\n---\n\n# H\n", &[]),
        ("---   \nfm\n---   \n# H\n", &[]),
        ("---\nfm\n...\n# H\n", &[1]),
        ("---\n---\n# H\n", &[]),
        ("+++\nfm\n+++\n# H\n", &[]),
        ("+++\nfm\n...\n# H\n", &[]),
        ("{\nfm\n}\n# H\n", &[]),
        ("{\nfm\n}", &[]),
        ("---\ntitle: x\n---\n## H\n", &[]),
        ("+++\ntitle = \"x\"\n+++\n## H\n", &[]),
        ("{\ntitle: x\n}\n## H\n", &[]),
        ("{\nno title\n}\n## H\n", &[4]),
        // A leading blank line puts the delimiter somewhere other than byte zero, and a `---` that
        // does not open the file is a setext underline or a thematic break.
        ("\n---\nfm\n---\n# H\n", &[2]),
        ("--- x\n---\nfoo\n---\n# H\n", &[1]),
        ("text\n---\nfm\n---\n# H\n", &[1]),
        ("---\r\nfm\r\n---\r\n# H\r\n", &[]),
        ("+++\r\nfm\r\n+++\r\n# H\r\n", &[]),
        ("\u{feff}# H\n", &[]),
    ];

    #[test]
    fn matches_markdownlint() {
        for &(source, expected) in CASES {
            assert_eq!(expected, lines(source).as_slice(), "source {source:?}");
        }
    }

    /// Three `---` lines in a row. markdownlint's pattern is lazy, so its front matter stops at the
    /// second and the third is a thematic break it reports; the parser here reads the middle line as
    /// the front matter's content and swallows all three.
    #[test]
    fn three_delimiter_lines_are_a_known_difference() {
        // markdownlint: [3]
        assert_eq!(Vec::<Line>::new(), lines("---\n---\n---\n# H\n"));
    }

    /// The front matter pattern markdownlint strips with, which the rule ports rather than asks the
    /// parser about — the parser is narrower, and MD041 has to agree with markdownlint and not with
    /// it. All three delimiter pairs, and only at byte zero.
    #[test]
    fn front_matter_pattern_matches_all_three_delimiters() {
        for source in [
            "---\nfm\n---\n",
            "+++\nfm\n+++\n",
            "+++\nfm\n...\n",
            "{\nfm\n}\n",
        ] {
            assert!(
                super::FRONT_MATTER
                    .find(source)
                    .is_some_and(|found| found.start() == 0 && found.end() == source.len()),
                "source {source:?}"
            );
        }
        for source in ["text\n---\nfm\n---\n", "--- x\n---\nfoo\n---\n"] {
            assert!(
                !super::FRONT_MATTER
                    .find(source)
                    .is_some_and(|found| found.start() == 0),
                "source {source:?}"
            );
        }
    }
}
