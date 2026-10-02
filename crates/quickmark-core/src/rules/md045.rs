use crate::ast::{Node, NodeRange};
use once_cell::sync::Lazy;
use regex::Regex;
use std::rc::Rc;

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

/// Finds `<img …>` inside an `html_block`, which comrak hands over as one opaque literal. Inline
/// HTML arrives as its own node, so this only ever runs over block-level HTML.
static IMG_TAG_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?si)<(/?)img\b[^>]*>").expect("Invalid img tag regex"));

/// markdownlint spells both attribute patterns `/\sNAME\s*=\s*['"]?([^'"\s>]*)/iu`. The leading
/// `\s` is load-bearing: a `\b` would accept `data-alt="x"` as an `alt` attribute, and markdownlint
/// reports that image while a word boundary would not.
static ALT_ATTRIBUTE_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)\salt\s*=\s*['"]?([^'"\s>]*)"#).expect("Invalid alt attribute regex")
});

static ARIA_HIDDEN_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?i)\saria-hidden\s*=\s*['"]?([^'"\s>]*)"#).expect("Invalid aria-hidden regex")
});

/// The tag name markdownlint's `getHtmlTagInfo` extracts with `/^<([^!>][^/\s>]*)/`, keeping a
/// leading `/` on a closing tag. `None` when the text is not a tag at all — `<!--`, `<!DOCTYPE`,
/// or the tail of a run-on literal.
fn html_tag_name(tag: &str) -> Option<&str> {
    let after_lt = tag.strip_prefix('<')?;
    let first = after_lt.chars().next()?;
    if first == '!' || first == '>' {
        return None;
    }
    let rest = &after_lt[first.len_utf8()..];
    let stop = rest
        .find(|c: char| c == '/' || c.is_whitespace() || c == '>')
        .unwrap_or(rest.len());
    Some(&after_lt[..first.len_utf8() + stop])
}

fn aria_hidden_is_true(tag: &str) -> bool {
    ARIA_HIDDEN_REGEX
        .captures(tag)
        .and_then(|captures| captures.get(1))
        .is_some_and(|value| value.as_str().eq_ignore_ascii_case("true"))
}

fn img_tag_missing_alt(tag: &str) -> bool {
    html_tag_name(tag).is_some_and(|name| {
        // A closing tag's name keeps its `/`, so `</img>` never reaches the attribute tests.
        name.eq_ignore_ascii_case("img")
            && !ALT_ATTRIBUTE_REGEX.is_match(tag)
            && !aria_hidden_is_true(tag)
    })
}

/// Spans of `<!-- … -->` inside an HTML block, which comrak hands over as one opaque literal.
/// micromark does not subtokenize a comment flow, so an `<img>` written inside one never becomes an
/// `htmlText` token and MD045 must not see it. A `<!--` with no later `-->` is deliberately not a
/// span: measured, markdownlint still reports an image in that case.
fn comment_spans(content: &str) -> Vec<(usize, usize)> {
    const OPEN: &str = "<!--";
    const CLOSE: &str = "-->";
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(offset) = content[at..].find(OPEN) {
        let start = at + offset;
        let after_open = start + OPEN.len();
        // No CLOSE after this OPEN means none after any later OPEN either.
        let Some(close) = content[after_open..].find(CLOSE) else {
            break;
        };
        let end = after_open + close + CLOSE.len();
        spans.push((start, end));
        at = end;
    }
    spans
}

fn find_html_image_violations(content: &str) -> Vec<(usize, usize)> {
    let comments = comment_spans(content);
    IMG_TAG_REGEX
        .find_iter(content)
        .filter(|tag| img_tag_missing_alt(tag.as_str()))
        .filter(|tag| {
            !comments
                .iter()
                .any(|&(start, end)| tag.start() >= start && tag.end() <= end)
        })
        .map(|tag| (tag.start(), tag.end()))
        .collect()
}

pub(crate) struct MD045Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    line_starts: Vec<usize>,
}

impl MD045Linter {
    pub fn new(context: Rc<Context>) -> Self {
        // Pre-calculate line starts for efficient line/col lookup
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(
                context
                    .document_content
                    .borrow()
                    .match_indices('\n')
                    .map(|(i, _)| i + 1),
            )
            .collect();

        Self {
            context,
            violations: Vec::new(),
            line_starts,
        }
    }

    /// Walks the inline subtree, which `feed` never descends into because inline kinds are filtered
    /// out of dispatch.
    fn feed_inline(&mut self, root: Node) {
        let mut cursor = root.walk();
        let mut depth = 0;
        loop {
            let node = cursor.node();
            let violation = match node.kind() {
                // comrak gives an image no children exactly when the label between `![` and `]` is
                // empty, which is markdownlint's entire test — a whitespace-only label keeps its
                // `text` child and is not a violation. An image whose reference never resolves is
                // not an `image` node in comrak or in micromark, so `![][undefined]` stays silent.
                "image" if node.child_count() == 0 => Some(node.range()),
                "html_inline" => {
                    let range = node.range();
                    let missing_alt = {
                        let content = self.context.document_content.borrow();
                        img_tag_missing_alt(&content[range.start_byte..range.end_byte])
                    };
                    missing_alt.then_some(range)
                }
                _ => None,
            };
            if let Some(range) = violation {
                self.add_violation(&range);
            }

            if cursor.goto_first_child() {
                depth += 1;
                continue;
            }
            loop {
                if depth == 0 {
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

    fn feed_html_block(&mut self, node: Node) {
        let (ranges, base) = {
            let content = self.context.document_content.borrow();
            (
                find_html_image_violations(&content[node.start_byte()..node.end_byte()]),
                node.start_byte(),
            )
        };
        for (start, end) in ranges {
            let range = self.range_at(base + start, base + end);
            self.add_violation(&range);
        }
    }

    fn add_violation(&mut self, range: &NodeRange) {
        let violation = RuleViolation::new(
            &MD045,
            MD045.description.to_string(),
            self.context.file_path.clone(),
            range_from_node_range(range),
        );
        self.violations.push(violation);
    }

    fn range_at(&self, start_byte: usize, end_byte: usize) -> NodeRange {
        let (start_row, start_col) = self.byte_to_line_col(start_byte);
        let (end_row, end_col) = self.byte_to_line_col(end_byte);
        NodeRange {
            start_byte,
            end_byte,
            start_point: crate::ast::Point {
                row: start_row,
                column: start_col,
            },
            end_point: crate::ast::Point {
                row: end_row,
                column: end_col,
            },
        }
    }

    fn byte_to_line_col(&self, byte_pos: usize) -> (usize, usize) {
        let line = match self.line_starts.binary_search(&byte_pos) {
            Ok(line) => line,
            Err(line) => line - 1,
        };
        let line_start = self.line_starts[line];
        let col = byte_pos - line_start;
        (line, col)
    }
}

pub const MD045: Rule = Rule {
    id: "MD045",
    alias: "no-alt-text",
    tags: &["accessibility", "images"],
    description: "Images should have alternate text (alt text)",
    rule_type: RuleType::Token,
    required_nodes: &["inline", "html_block"],
    new_linter: |context| Box::new(MD045Linter::new(context)),
};

impl RuleLinter for MD045Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            "inline" => self.feed_inline(*node),
            "html_block" => self.feed_html_block(*node),
            _ => {}
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("no-alt-text", RuleSeverity::Error),
            ("no-inline-html", RuleSeverity::Off),
        ])
    }

    fn md045_count(input: &str) -> usize {
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
        linter
            .analyze()
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .count()
    }

    #[test]
    fn test_markdown_images_with_alt_text_no_violations() {
        let input = "# Test\n\n![Valid alt text](image.jpg)\n\n![Another valid image](image.jpg \"Title\")\n\n![Reference image with alt][ref]\n\nReference image with alt text ![Alt text reference][ref2]\n\n[ref]: image.jpg\n[ref2]: image.jpg \"Title\"\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();
        assert_eq!(md045_violations.len(), 0);
    }

    #[test]
    fn test_markdown_images_without_alt_text_violations() {
        let input = "# Test\n\n![](image.jpg)\n\n![](image.jpg \"Title\")\n\n![Empty alt](image.jpg) and ![](inline-image.jpg) in text\n\nReference image without alt ![][ref]\n\n[ref]: image.jpg\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();

        // Should find 4 violations:
        // Line 2: ![](image.jpg)
        // Line 4: ![](image.jpg "Title")
        // Line 6: ![](inline-image.jpg)
        // Line 8: ![][ref]
        assert_eq!(md045_violations.len(), 4);
    }

    #[test]
    fn test_html_images_with_alt_attribute_no_violations() {
        let input = "# Test\n\n<img src=\"image.jpg\" alt=\"Valid alt text\" />\n\n<img src=\"image.jpg\" alt=\"Another valid\" >\n\n<IMG SRC=\"image.jpg\" ALT=\"Case insensitive\" />\n\n<img \n  src=\"image.jpg\" \n  alt=\"Multi-line\" \n  />\n\n<img src=\"image.jpg\" alt=\"\" />\n\n<img src=\"image.jpg\" alt='' />\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();
        assert_eq!(md045_violations.len(), 0);
    }

    #[test]
    fn test_html_images_without_alt_attribute_violations() {
        let input = "# Test\n\n<img src=\"image.jpg\" />\n\n<img src=\"image.jpg\" alt>\n\n<IMG SRC=\"image.jpg\" />\n\n<img \n  src=\"image.jpg\" \n  title=\"Title only\" />\n\n<p><img src=\"nested.jpg\"></p>\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();

        // Should find 5 violations:
        // Line 2: <img src="image.jpg" />
        // Line 4: <img src="image.jpg" alt>
        // Line 6: <IMG SRC="image.jpg" />
        // Line 8-10: Multi-line img tag
        // Line 12: nested img tag
        assert_eq!(md045_violations.len(), 5);
    }

    #[test]
    fn test_html_images_with_aria_hidden_no_violations() {
        let input = "# Test\n\n<img src=\"image.jpg\" aria-hidden=\"true\" />\n\n<img src=\"image.jpg\" ARIA-HIDDEN=\"TRUE\" />\n\n<img \n  src=\"image.jpg\" \n  aria-hidden=\"true\"
  />\n\n<img src=\"image.jpg\" aria-hidden='true' />\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();
        assert_eq!(md045_violations.len(), 0);
    }

    #[test]
    fn test_html_images_with_aria_hidden_false_violations() {
        let input = "# Test\n\n<img src=\"image.jpg\" aria-hidden=\"false\" />\n\n<img src=\"image.jpg\" aria-hidden=\"\" />\n\n<img src=\"image.jpg\" aria-hidden=\"other\" />\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();

        // Should find 3 violations (aria-hidden != \"true\")
        assert_eq!(md045_violations.len(), 3);
    }

    #[test]
    fn test_mixed_image_types() {
        let input = "# Test\n\n![Valid alt](image.jpg)\n\n![](no-alt.jpg)\n\n<img src=\"valid.jpg\" alt=\"Valid\" />\n\n<img src=\"no-alt.jpg\" />\n\n<img src=\"hidden.jpg\" aria-hidden=\"true\" />\n\n![Reference valid][ref1]\n\n![][ref2]\n\n[ref1]: image.jpg\n[ref2]: image.jpg\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();

        // Should find 3 violations:
        // Line 4: ![](no-alt.jpg)
        // Line 8: <img src="no-alt.jpg" />
        // Line 14: ![][ref2]
        assert_eq!(md045_violations.len(), 3);
    }

    #[test]
    fn test_multiline_markdown_images() {
        let input = "# Test\n\n![Alt text](image.jpg 
\"Title\")\n\n![](image.jpg 
\"Title\")\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();

        // Should find 1 violation (the second image without alt text)
        assert_eq!(md045_violations.len(), 1);
    }

    #[test]
    fn test_images_in_links() {
        let input = "# Test\n\n[![Alt text](image.jpg)](link.html)\n\n[![](no-alt.jpg)](link.html)\n\n[<img src=\"alt.jpg\" alt=\"Alt\" />](link.html)\n\n[<img src=\"no-alt.jpg\" />](link.html)\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();

        // Should find 2 violations:
        // Line 4: [![](no-alt.jpg)](link.html) - markdown image without alt
        // Line 8: [<img src="no-alt.jpg" />](link.html) - HTML img without alt
        assert_eq!(md045_violations.len(), 2);
    }

    #[test]
    fn test_no_false_positives_in_code_blocks() {
        let input = "# Test\n\n```html\n![](image.jpg)\n<img src=\"image.jpg\" />\n```\n\n    ![](indented-code.jpg)\n    <img src=\"indented.jpg\" />\n\n`![](inline-code.jpg)` and `<img src=\"inline.jpg\" />`\n\nRegular text with ![](actual-image.jpg) should trigger.\n";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md045_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD045")
            .collect();

        // Should only find 1 violation (the actual image outside code blocks)
        assert_eq!(md045_violations.len(), 1);
    }

    /// Every expectation below is a markdownlint-cli2 0.23.3 (markdownlint 0.41.1) measurement with
    /// `{"default": false, "no-alt-text": true}`, one fixture per assertion.
    #[test]
    fn test_empty_alt_shapes_measured_against_markdownlint() {
        // An inline image with an empty destination is still an image.
        assert_eq!(md045_count("![]()\n"), 1);
        assert_eq!(md045_count("![](/a.png \"t\")\n"), 1);
        assert_eq!(md045_count("![](<a.png>)\n"), 1);
        assert_eq!(md045_count("x ![](  /a.png  ) y\n"), 1);
        assert_eq!(md045_count("![](/a.png\n)\n"), 1);

        // A label holding only whitespace is alt text as far as markdownlint is concerned.
        assert_eq!(md045_count("![ ](/a.png)\n"), 0);
        assert_eq!(md045_count("![\t](/a.png)\n"), 0);
        assert_eq!(md045_count("![&nbsp;](/a.png)\n"), 0);

        // Balanced brackets and escapes stay inside the label.
        assert_eq!(md045_count("![a[b]c](/x.png)\n"), 0);
        assert_eq!(md045_count("![a\\]](/x.png)\n"), 0);

        // A link, not an image.
        assert_eq!(md045_count("[](/a.png)\n"), 0);
    }

    #[test]
    fn test_reference_images_only_count_when_the_reference_resolves() {
        // `![][]` is not an image at all — an empty label cannot be a reference.
        assert_eq!(md045_count("![][]\n"), 0);
        assert_eq!(md045_count("![alt][]\n"), 0);
        assert_eq!(md045_count("![][ ]\n"), 0);
        assert_eq!(md045_count("![ ][]\n"), 0);

        // An unresolved reference is literal text, so there is no image to report.
        assert_eq!(md045_count("![][ref]\n"), 0);

        // Resolved, so the empty label is a violation. Lookup is case-folded.
        assert_eq!(md045_count("![][ref]\n\n[ref]: /i.png\n"), 1);
        assert_eq!(md045_count("![][REF]\n\n[ref]: /i.png\n"), 1);
        assert_eq!(md045_count("![][é]\n\n[é]: /i.png\n"), 1);

        // A resolved reference whose label has alt text.
        assert_eq!(md045_count("![a][ref]\n\n[ref]: /i.png\n"), 0);
    }

    #[test]
    fn test_html_img_attribute_shapes_measured_against_markdownlint() {
        // `data-alt` is not `alt`; markdownlint's pattern anchors on preceding whitespace.
        assert_eq!(md045_count("<img src=\"a.png\" data-alt=\"x\">\n"), 1);
        assert_eq!(md045_count("<img src=\"a.png\" alt=x>\n"), 0);
        assert_eq!(md045_count("<img src=\"a.png\" alt=\"\">\n"), 0);
        assert_eq!(md045_count("<img src=\"a.png\" alt>\n"), 1);
        assert_eq!(md045_count("<img\n  src=\"a.png\"\n  alt=\"y\">\n"), 0);
        assert_eq!(md045_count("<IMG SRC=\"a.png\">\n"), 1);
        assert_eq!(md045_count("</img>\n"), 0);
        assert_eq!(md045_count("<imgx src=\"a.png\">\n"), 0);
        assert_eq!(md045_count("<img src=\"a.png\" aria-hidden=\"TRUE\">\n"), 0);
        assert_eq!(
            md045_count("<img src=\"a.png\" aria-hidden=\"false\">\n"),
            1
        );

        // Inline, in a paragraph and inside an HTML block alike.
        assert_eq!(md045_count("text <img src=\"a.png\"> text\n"), 1);
        assert_eq!(md045_count("<div><img src=\"a.png\"></div>\n"), 1);
        assert_eq!(md045_count("<div>\n<img src=\"a.png\">\n</div>\n"), 1);

        // A code span hides it.
        assert_eq!(md045_count("`<img src=\"a.png\">`\n"), 0);
    }

    /// micromark does not subtokenize a comment flow, so an `<img>` inside `<!-- … -->` is not an
    /// image. A `<!--` that is never closed is not a comment as far as markdownlint is concerned
    /// either — it still reports the image.
    #[test]
    fn test_img_inside_html_comment_is_not_an_image() {
        assert_eq!(md045_count("<!-- <img src=\"a.png\"> -->\n"), 0);
        assert_eq!(md045_count("<!--\n<img src=\"a.png\">\n-->\n"), 0);
        assert_eq!(md045_count("<!-- <!-- <img src=\"a.png\"> --> -->\n"), 0);
        assert_eq!(md045_count("<div><!-- <img src=\"a.png\"> --></div>\n"), 0);
        assert_eq!(md045_count("<!--- <img src=\"a.png\"> --->\n"), 0);

        assert_eq!(md045_count("<!-- unclosed\n<img src=\"a.png\">\n"), 1);
        assert_eq!(md045_count("<!-- c -->\n<img src=\"a.png\">\n"), 1);
        assert_eq!(
            md045_count("<img src=\"a.png\">\n<!-- <img src=\"b.png\"> -->\n"),
            1
        );
        assert_eq!(
            md045_count("<!-- <img src=\"a.png\"> -->\n<img src=\"b.png\">\n"),
            1
        );

        // An inline comment already arrives as one `html_inline` node whose name starts with `!`.
        assert_eq!(md045_count("text <!-- <img src=\"a.png\"> --> text\n"), 0);
    }
}
