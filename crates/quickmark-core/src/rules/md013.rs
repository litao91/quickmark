use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::Node;
use linkify::{LinkFinder, LinkKind};

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{
        md034::{gfm_emails, is_gfm_autolink},
        Context, Rule, RuleLinter, RuleType,
    },
};

// MD013-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize)]
#[serde(default)]
pub struct MD013LineLengthTable {
    pub line_length: usize,
    pub code_block_line_length: Option<usize>,
    pub heading_line_length: Option<usize>,
    pub code_blocks: bool,
    pub headings: bool,
    pub tables: bool,
    pub strict: bool,
    pub stern: bool,
}

impl Default for MD013LineLengthTable {
    fn default() -> Self {
        Self {
            line_length: 80,
            code_block_line_length: None,
            heading_line_length: None,
            code_blocks: true,
            headings: true,
            tables: true,
            strict: false,
            stern: false,
        }
    }
}

impl MD013LineLengthTable {
    /// The three limits markdownlint measures against, as `(plain, code block, heading)`.
    ///
    /// It reads each configured limit as `config.X || fallback`, so a limit that was not set does not
    /// stand on its own: `line_length` falls back to its documented 80, and the other two fall back to
    /// `line_length`. That last step is why setting only `line_length` shortens headings and code
    /// blocks too.
    pub fn limits(&self) -> (usize, usize, usize) {
        let plain = or_fallback(self.line_length, 80);
        (
            plain,
            or_fallback_opt(self.code_block_line_length, plain),
            or_fallback_opt(self.heading_line_length, plain),
        )
    }
}

fn or_fallback(limit: usize, fallback: usize) -> usize {
    if limit == 0 {
        fallback
    } else {
        limit
    }
}

fn or_fallback_opt(limit: Option<usize>, fallback: usize) -> usize {
    limit.map_or(fallback, |limit| or_fallback(limit, fallback))
}

/// MD013 Line Length Rule Linter
///
/// **SINGLE-USE CONTRACT**: This linter is designed for one-time use only.
/// After processing a document (via feed() calls and finalize()), the linter
/// should be discarded. The pending_violations state is not cleared between uses.
pub(crate) struct MD013Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    heading_lines: HashSet<usize>,
    code_lines: HashSet<usize>,
    table_lines: HashSet<usize>,
    front_matter_lines: HashSet<usize>,
    /// Rows a `link` or `image` covers.
    link_lines: HashSet<usize>,
    /// Rows a paragraph's own text covers.
    paragraph_text_lines: HashSet<usize>,
    definition_lines: HashSet<usize>,
    finder: LinkFinder,
}

impl MD013Linter {
    pub fn new(context: Rc<Context>) -> Self {
        // GFM autolinks a scheme-less `www.example.com`, which linkify only looks for when told to.
        // Everything else it would find that way is literal text, so `is_gfm_autolink` filters it
        // back out.
        let mut finder = LinkFinder::new();
        finder.url_must_have_scheme(false);
        Self {
            context,
            violations: Vec::new(),
            heading_lines: HashSet::new(),
            code_lines: HashSet::new(),
            table_lines: HashSet::new(),
            front_matter_lines: HashSet::new(),
            link_lines: HashSet::new(),
            paragraph_text_lines: HashSet::new(),
            definition_lines: HashSet::new(),
            finder,
        }
    }

    /// Records every 0-based line the node covers. A block's end row is the row *after* its last
    /// content row, because block ends swallow the trailing newline; at EOF without one the end row
    /// is the last content row itself. markdownlint's tokens end on their last content line, so the
    /// two need this compensation to agree.
    fn cover(lines: &mut HashSet<usize>, node: &Node) {
        let start = node.start_position().row;
        let end = node.end_position();
        let last = if end.column == 0 {
            end.row.saturating_sub(1).max(start)
        } else {
            end.row
        };
        lines.extend(start..=last);
    }

    /// markdownlint builds two line sets out of micromark's tokens: the lines a link or image covers,
    /// and the lines a *paragraph's* own text covers. A line in the first but not the second is
    /// nothing but links, and the default and stern modes spare it because there is no prose to wrap.
    ///
    /// Two wrinkles. A setext heading's text sits under a paragraph this facade synthesizes, but
    /// micromark's setext heading is not a paragraph token, so its text does not count. And comrak
    /// leaves a bare URL as ordinary text where micromark's autolink-literal extension makes it a
    /// link, so those spans are found here and taken back out of the paragraph's text.
    fn collect_inline(&mut self, inline: &Node) {
        let counts_as_paragraph = inline.parent().is_some_and(|parent| {
            parent.kind() == "paragraph"
                && parent
                    .parent()
                    .is_none_or(|grandparent| grandparent.kind() != "setext_heading")
        });
        let context = Rc::clone(&self.context);
        let source = context.document_content.borrow();

        // markdownlint reads a paragraph's `data` tokens through
        // `getDescendantsByType(paragraph, ["data"])`, which descends exactly one level. Only the
        // inline's own text children count, so text nested in an emphasis does not — and that is what
        // makes a paragraph holding nothing but a link come out as "link only".
        for index in 0..inline.child_count() {
            let Some(child) = inline.child(index) else {
                continue;
            };
            if child.kind() != "text" {
                continue;
            }
            let text = &source[child.start_byte()..child.end_byte()];
            let autolinks = self.autolinks(text);
            if counts_as_paragraph && has_text_outside(text, &autolinks) {
                Self::cover(&mut self.paragraph_text_lines, &child);
            }
        }

        // Links and images count wherever they sit, and micromark runs its autolink-literal extension
        // over any text that is not inside a link or image label.
        let mut stack = vec![*inline];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "link" | "image" => Self::cover(&mut self.link_lines, &node),
                "text" => {
                    let text = &source[node.start_byte()..node.end_byte()];
                    if !self.autolinks(text).is_empty() {
                        Self::cover(&mut self.link_lines, &node);
                    }
                }
                _ => {
                    for index in 0..node.child_count() {
                        if let Some(child) = node.child(index) {
                            stack.push(child);
                        }
                    }
                }
            }
        }
    }

    /// The GFM autolink literals in `text`, as sorted non-overlapping byte spans.
    fn autolinks(&self, text: &str) -> Vec<(usize, usize)> {
        let mut spans: Vec<(usize, usize)> = self
            .finder
            .links(text)
            .filter(|link| *link.kind() == LinkKind::Email || is_gfm_autolink(link.as_str()))
            .map(|link| (link.start(), link.end()))
            // linkify reads `oss://key:secret@host/path` as one URL whose scheme GFM does not
            // autolink, so the address inside it is invisible to the finder above. micromark's
            // `literalAutolink` covers it, which is what makes a long table cell holding an
            // `oss://` credential nothing but a link.
            .chain(gfm_emails(text))
            .collect();
        spans.sort_unstable();
        // `has_text_outside` walks the spans in order, so one that starts inside its predecessor has
        // to be folded into it rather than left to move the cursor backwards.
        spans.dedup_by(|later, kept| {
            if later.0 <= kept.1 {
                kept.1 = kept.1.max(later.1);
                return true;
            }
            false
        });
        spans
    }

    /// Runs once every node has been fed, so it lives in `finalize` rather than in `feed`.
    fn analyze_all_lines(&mut self) {
        let settings = &self.context.config.linters.settings.line_length;
        let (plain_limit, code_limit, heading_limit) = settings.limits();
        let lines = self.context.lines.borrow();

        for (line_index, line) in lines.iter().enumerate() {
            if self.front_matter_lines.contains(&line_index) {
                continue;
            }
            let in_code = self.code_lines.contains(&line_index);
            let is_heading = self.heading_lines.contains(&line_index);
            let in_table = self.table_lines.contains(&line_index);

            if (in_code && !settings.code_blocks)
                || (is_heading && !settings.headings)
                || (in_table && !settings.tables)
            {
                continue;
            }

            // markdownlint's precedence: a code line is measured against the code limit even when it
            // is also something else, then headings, then everything else at the plain limit.
            let limit = if in_code {
                code_limit
            } else if is_heading {
                heading_limit
            } else {
                plain_limit
            };

            if self.should_violate_line(line_index, line, limit) {
                let violation = self.create_violation_for_line(line, line_index, limit);
                self.violations.push(violation);
            }
        }
    }

    /// Whether a line's content is nothing but links, which markdownlint spares outside strict mode
    /// because there is no prose in it to wrap.
    fn is_link_only(&self, line_index: usize) -> bool {
        self.link_lines.contains(&line_index) && !self.paragraph_text_lines.contains(&line_index)
    }

    fn should_violate_line(&self, line_index: usize, line: &str, limit: usize) -> bool {
        let settings = &self.context.config.linters.settings.line_length;

        // A link reference definition is exempt in every mode.
        if self.definition_lines.contains(&line_index) {
            return false;
        }

        let length = utf16_len(line);

        // Strict mode measures the line as it stands and takes no further exception.
        if settings.strict {
            return length > limit;
        }

        if self.is_link_only(line_index) {
            return false;
        }

        // Stern mode measures the line as it stands too, but spares one there is nowhere to wrap.
        if settings.stern {
            return length > limit && !not_wrappable(line);
        }

        // The default mode measures the line with its last run of non-whitespace folded to a single
        // character, and then reports nothing when the real length equals the limit: markdownlint
        // routes this through `addErrorDetailIf`, which stays quiet when expected equals actual.
        folded_len(line) > limit && length != limit
    }

    fn create_violation_for_line(
        &self,
        line: &str,
        line_number: usize,
        limit: usize,
    ) -> RuleViolation {
        // markdownlint points at the column just past the limit and runs to the end of the line,
        // counting both in UTF-16 units; `byte_at_unit` translates the start into the bytes this
        // range reports.
        let column = byte_at_unit(line, limit);
        RuleViolation::new(
            &MD013,
            format!(
                "{} [Expected: {}; Actual: {}]",
                MD013.description,
                limit,
                utf16_len(line)
            ),
            self.context.file_path.clone(),
            range_from_node_range(&crate::ast::NodeRange {
                start_byte: 0,
                end_byte: line.len(),
                start_point: crate::ast::Point {
                    row: line_number,
                    column,
                },
                end_point: crate::ast::Point {
                    row: line_number,
                    column: line.len(),
                },
            }),
        )
    }
}

/// JavaScript's `String.length`, which is what markdownlint measures every MD013 length in: UTF-16
/// code units, so a character outside the basic multilingual plane counts twice.
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// JavaScript's `\s`, which markdownlint's `/\S*$/u` and `notWrappableRe` are written against. It
/// differs from Rust's `char::is_whitespace` on two characters: U+0085 is whitespace there and not
/// here, and U+FEFF is whitespace here and not there.
fn is_js_whitespace(character: char) -> bool {
    (character.is_whitespace() && character != '\u{85}') || character == '\u{feff}'
}

/// markdownlint's `line.replace(/\S*$/u, "#").length`: the line with its last run of non-whitespace
/// folded to a single character. That is how "the last word may hang over the limit as long as it
/// starts inside it" works — `aaa bbb` against a limit of four becomes `aaa #` and is still too long,
/// while one unbreakable run becomes `#` and is not. A line ending in whitespace has an empty run, so
/// it grows by one.
fn folded_len(line: &str) -> usize {
    let run: usize = line
        .chars()
        .rev()
        .take_while(|&character| !is_js_whitespace(character))
        .map(char::len_utf16)
        .sum();
    utf16_len(line) - run + 1
}

/// markdownlint's `notWrappableRe`, `/^(?:[#>\s]*\s)?\S*$/u`, which stern mode uses to spare a line
/// there is nowhere to wrap. The group can only end at the line's last whitespace, because `\S*$`
/// forbids another one after it, so the question is whether everything before that last whitespace is
/// heading markers, quote markers or whitespace — or whether the line has no whitespace at all.
fn not_wrappable(line: &str) -> bool {
    match line
        .char_indices()
        .rev()
        .find(|&(_, character)| is_js_whitespace(character))
    {
        // No whitespace at all: the whole line is one unbreakable run.
        None => true,
        Some((last, _)) => line[..last]
            .chars()
            .all(|character| character == '#' || character == '>' || is_js_whitespace(character)),
    }
}

/// Whether anything in `text` sits outside `spans`. micromark emits a `data` token for whatever falls
/// between two autolink literals, however short — a single space is enough — so a line holding two
/// URLs is not "nothing but links" and gets measured like any other.
fn has_text_outside(text: &str, spans: &[(usize, usize)]) -> bool {
    let mut cursor = 0;
    for &(from, to) in spans {
        if from > cursor {
            return true;
        }
        cursor = to;
    }
    cursor < text.len()
}

/// The byte offset of a UTF-16 code-unit index, clamped to the line.
fn byte_at_unit(line: &str, units: usize) -> usize {
    let mut seen = 0;
    for (offset, character) in line.char_indices() {
        if seen >= units {
            return offset;
        }
        seen += character.len_utf16();
    }
    line.len()
}

impl RuleLinter for MD013Linter {
    fn feed(&mut self, node: &Node) {
        match node.kind() {
            "atx_heading" | "setext_heading" => Self::cover(&mut self.heading_lines, node),
            "fenced_code_block" | "indented_code_block" => Self::cover(&mut self.code_lines, node),
            "pipe_table" => Self::cover(&mut self.table_lines, node),
            // markdownlint strips front matter from the content before it parses, so no rule ever
            // sees those lines and reported line numbers are shifted back afterwards.
            "minus_metadata" | "plus_metadata" => Self::cover(&mut self.front_matter_lines, node),
            // markdownlint's `definitionLineIndices` covers both `definition` and
            // `gfmFootnoteDefinition` tokens, over every line each one spans.
            "link_reference_definition" | "footnote_definition" => {
                Self::cover(&mut self.definition_lines, node)
            }
            "inline" => self.collect_inline(node),
            _ => {}
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        self.analyze_all_lines();
        std::mem::take(&mut self.violations)
    }
}

pub const MD013: Rule = Rule {
    id: "MD013",
    aliases: &["line-length"],
    tags: &["line_length"],
    description: "Line length",
    rule_type: RuleType::Line,
    required_nodes: &[
        "atx_heading",
        "setext_heading",
        "fenced_code_block",
        "indented_code_block",
        "pipe_table",
        "minus_metadata",
        "plus_metadata",
        "link_reference_definition",
        "footnote_definition",
        "inline",
    ],
    new_linter: |context| Box::new(MD013Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD013LineLengthTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::{test_config_with_rules, test_config_with_settings};

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("line-length", RuleSeverity::Error),
            ("heading-style", RuleSeverity::Off),
            ("heading-increment", RuleSeverity::Off),
        ])
    }

    fn test_config_with_line_length(
        line_length_config: MD013LineLengthTable,
    ) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![
                ("line-length", RuleSeverity::Error),
                ("heading-style", RuleSeverity::Off),
                ("heading-increment", RuleSeverity::Off),
            ],
            LintersSettingsTable {
                line_length: line_length_config,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_line_length_violation() {
        let input = "This is a line that is definitely longer than eighty characters and should trigger a violation.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());

        let violation = &violations[0];
        assert_eq!("MD013", violation.rule().id);
        assert!(violation.message().contains("Expected: 80"));
        assert!(violation
            .message()
            .contains(&format!("Actual: {}", input.len())));
    }

    #[test]
    fn test_line_length_no_violation() {
        let mut input =
            "This line should be exactly eighty characters long and not trigger".to_string();
        while input.len() < 80 {
            input.push('x');
        }
        assert_eq!(80, input.len());

        let config = test_config();
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, &input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_link_reference_definition_exception() {
        let input = "[very-long-link-reference-that-exceeds-eighty-characters]: https://example.com/very-long-url-that-should-be-exempted";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_standalone_link_exception() {
        let input = "[This is a very long link text that definitely exceeds eighty characters](https://example.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_standalone_image_exception() {
        let input = "![This is a very long image alt text that definitely exceeds eighty characters](https://example.com/image.jpg)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_no_spaces_beyond_limit_exception() {
        let input = "This line has exactly eighty characters and then continues without spaces: https://example.com/very-long-url-without-spaces";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_spaces_beyond_limit_violation() {
        // Create a string that exceeds 80 chars with a space beyond the limit
        let mut input =
            "This line has exactly eighty characters and should trigger violation".to_string();
        while input.len() < 80 {
            input.push('x');
        }
        input.push(' '); // Add space beyond limit

        let config = test_config();
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, &input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
    }

    #[test]
    fn test_strict_mode() {
        let line_length_config = MD013LineLengthTable {
            strict: true,
            ..MD013LineLengthTable::default()
        };

        let input = "This line has exactly eighty characters and then continues without spaces like: https://example.com/url";

        let config = test_config_with_line_length(line_length_config);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len()); // Should violate in strict mode
    }

    #[test]
    fn test_stern_mode_with_spaces_beyond_limit() {
        let config = MD013LineLengthTable {
            stern: true,
            ..MD013LineLengthTable::default()
        };

        // Line with spaces beyond limit - should violate in stern mode
        // Make sure the line has exactly 80 chars, then add text with spaces beyond that
        let mut input =
            "This line has exactly eighty characters and should trigger violations".to_string();
        while input.len() < 80 {
            input.push('x');
        }
        input.push_str(" with spaces"); // Add spaces beyond limit

        let full_config = test_config_with_line_length(config);
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), full_config, &input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len()); // Should violate in stern mode
    }

    /// Stern mode measures the line as it stands, so a long final word no longer saves it — only a
    /// line `notWrappableRe` matches does, and this one has prose before its URL. Measured against
    /// markdownlint-cli2 v0.23.3, which reports `Actual: 123`.
    #[test]
    fn test_stern_mode_reports_a_long_trailing_word() {
        let config = MD013LineLengthTable {
            stern: true,
            ..MD013LineLengthTable::default()
        };

        let input = "This line has exactly eighty characters and then continues without spaces: https://example.com/very-long-url-without-spaces";

        let full_config = test_config_with_line_length(config);
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), full_config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("Actual: 123"));
    }

    #[test]
    fn test_stern_mode_vs_default_mode() {
        // Create line that exceeds limit with spaces beyond limit
        let mut input =
            "This line has exactly eighty characters and then continues with".to_string();
        while input.len() < 80 {
            input.push('x');
        }
        input.push_str(" spaces beyond"); // Add spaces beyond limit

        // Default mode - should violate because there are spaces beyond limit
        let default_config = MD013LineLengthTable::default();
        let default_full_config = test_config_with_line_length(default_config);
        let mut default_linter = MultiRuleLinter::new_for_document(
            PathBuf::from("test.md"),
            default_full_config,
            &input,
        );
        let default_violations = default_linter.analyze();

        // Stern mode - should violate because it's more aggressive about lines with spaces
        let stern_config = MD013LineLengthTable {
            stern: true,
            ..MD013LineLengthTable::default()
        };
        let stern_full_config = test_config_with_line_length(stern_config);
        let mut stern_linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), stern_full_config, &input);
        let stern_violations = stern_linter.analyze();

        // Both should catch this since it has spaces beyond limit
        assert_eq!(1, default_violations.len()); // Default should catch this since it has spaces
        assert_eq!(1, stern_violations.len()); // Stern should definitely catch this
    }

    #[test]
    fn test_stern_vs_strict_vs_default_comprehensive() {
        // Case 1: Line with spaces beyond limit - all modes should catch this
        let mut case1 =
            "This line has exactly eighty characters and then continues with".to_string();
        while case1.len() < 80 {
            case1.push('x');
        }
        case1.push_str(" spaces"); // Add spaces beyond limit

        // Case 2: a long final word, which the default mode folds away but stern and strict do not
        let case2 = "This line has exactly eighty characters and then continues without spaces: https://example.com/url".to_string();

        // Case 3: Line within limit - no mode should catch this
        let case3 = "This line is within the eighty character limit".to_string();

        let test_cases = vec![
            (&case1, true, true, true),    // Has spaces beyond limit
            (&case2, false, true, true),   // Long final word, prose before it
            (&case3, false, false, false), // Within limit
        ];

        for (input, expect_default, expect_stern, expect_strict) in test_cases {
            // Default mode
            let default_config = MD013LineLengthTable::default();
            let default_full_config = test_config_with_line_length(default_config);
            let mut default_linter = MultiRuleLinter::new_for_document(
                PathBuf::from("test.md"),
                default_full_config,
                input,
            );
            let default_violations = default_linter.analyze();
            assert_eq!(
                expect_default,
                !default_violations.is_empty(),
                "Default mode failed for: {input}"
            );

            // Stern mode
            let stern_config = MD013LineLengthTable {
                stern: true,
                ..MD013LineLengthTable::default()
            };
            let stern_full_config = test_config_with_line_length(stern_config);
            let mut stern_linter = MultiRuleLinter::new_for_document(
                PathBuf::from("test.md"),
                stern_full_config,
                input,
            );
            let stern_violations = stern_linter.analyze();
            assert_eq!(
                expect_stern,
                !stern_violations.is_empty(),
                "Stern mode failed for: {input}"
            );

            // Strict mode
            let strict_config = MD013LineLengthTable {
                strict: true,
                ..MD013LineLengthTable::default()
            };
            let strict_full_config = test_config_with_line_length(strict_config);
            let mut strict_linter = MultiRuleLinter::new_for_document(
                PathBuf::from("test.md"),
                strict_full_config,
                input,
            );
            let strict_violations = strict_linter.analyze();
            assert_eq!(
                expect_strict,
                !strict_violations.is_empty(),
                "Strict mode failed for: {input}"
            );
        }
    }

    #[test]
    fn test_custom_line_length() {
        let line_length_config = MD013LineLengthTable {
            line_length: 50,
            ..MD013LineLengthTable::default()
        };

        let input = "This line is longer than fifty characters and should violate";

        let config = test_config_with_line_length(line_length_config);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("Expected: 50"));
    }

    /// markdownlint reads each limit as `config.X || fallback`, so a config that sets only
    /// `line_length` shortens headings and code blocks as well. Measured against markdownlint-cli2
    /// v0.23.3, which reports both lines below with `Expected: 20`.
    #[test]
    fn an_unset_heading_or_code_limit_follows_line_length() {
        assert_eq!((80, 80, 80), MD013LineLengthTable::default().limits());
        let table = MD013LineLengthTable {
            line_length: 20,
            ..MD013LineLengthTable::default()
        };
        assert_eq!((20, 20, 20), table.limits());

        let input = "# heading text that is quite long indeed\n\n```\ncode text that is quite long indeed\n```\n";
        let config = test_config_with_line_length(table);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(2, violations.len());
        assert!(violations
            .iter()
            .all(|violation| violation.message().contains("Expected: 20")));
    }

    /// Stern mode spares a line markdownlint's `notWrappableRe` matches — an optional run of heading
    /// markers, quote markers and whitespace closed by one space, then nothing but non-whitespace —
    /// because there is nowhere to wrap it. Strict mode spares nothing. Every expectation is a
    /// markdownlint-cli2 v0.23.3 measurement at `line_length = 20`.
    #[test]
    fn stern_spares_a_line_there_is_nowhere_to_wrap() {
        fn reported(input: &str, stern: bool, strict: bool) -> Vec<usize> {
            let config = test_config_with_line_length(MD013LineLengthTable {
                line_length: 20,
                stern,
                strict,
                ..MD013LineLengthTable::default()
            });
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            linter
                .analyze()
                .iter()
                .map(|violation| violation.location().range.start.line + 1)
                .collect()
        }

        let run = "a".repeat(40);
        let unwrappable = [
            format!("# {run}\n"),
            format!("> {run}\n"),
            format!("###   {run}\n"),
            format!("  {run}\n"),
            format!("{run}aaaaa\n"),
        ];
        let wrappable = format!("word {run}\n");
        let both = || unwrappable.iter().chain(std::iter::once(&wrappable));

        // The default mode folds the last run away, so none of these is over the limit.
        for input in both() {
            assert!(reported(input, false, false).is_empty(), "{input:?}");
        }
        // Stern measures the line as it stands but spares the ones it cannot wrap.
        for input in &unwrappable {
            assert!(reported(input, true, false).is_empty(), "{input:?}");
        }
        assert_eq!(vec![1], reported(&wrappable, true, false));
        // Strict spares nothing.
        for input in both() {
            assert_eq!(vec![1], reported(input, false, true), "{input:?}");
        }
    }

    /// The report starts just past the limit. markdownlint counts that column in UTF-16 units and
    /// quickmark's ranges count bytes, as every other rule's do, so the two agree on ASCII and differ
    /// on a line of multi-byte characters by exactly the width those characters add.
    #[test]
    fn a_report_starts_just_past_the_limit() {
        fn column(input: &str) -> usize {
            let config = test_config_with_line_length(MD013LineLengthTable {
                line_length: 20,
                ..MD013LineLengthTable::default()
            });
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            let violations = linter.analyze();
            assert_eq!(1, violations.len(), "{input:?}");
            violations[0].location().range.start.character
        }

        assert_eq!(20, column("aaa bbb ccc ddd eee fff\n"));
        // Twenty UTF-16 units of `日本語 ` is five groups of ten bytes.
        let cjk = "日本語 ".repeat(8) + "\n";
        assert_eq!(50, column(&cjk));
    }

    /// markdownlint spares a line whose only content is links, because there is no prose in it to
    /// wrap. It reads a paragraph's `data` tokens through `getDescendantsByType(paragraph, ["data"])`,
    /// which descends exactly one level, so a link's own label is not prose but a space between two
    /// links is — and a bare URL counts as a link, because micromark's autolink-literal extension
    /// makes it one where comrak leaves it as text. Every expectation is a markdownlint-cli2 v0.23.3
    /// measurement at the default limit of 80.
    #[test]
    fn a_line_of_nothing_but_links_is_spared() {
        fn reports(input: &str) -> Vec<usize> {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
            linter
                .analyze()
                .iter()
                .map(|violation| violation.location().range.start.line + 1)
                .collect()
        }

        let url =
            "https://code.byted.org/inf/kafka/merge_requests/68/diffs/with/a/really/long/tail";
        let label = "Support SASL/PLAIN authentication mechanism for Kafka broker (!68) · GitLab";

        // Folding alone would not spare any of these: the last run is a pipe or a closing
        // parenthesis, so the line still measures over the limit.
        assert!(reports(&format!("| a | {url} |\n| - | - |\n")).is_empty());
        assert!(reports(&format!("- item\n  - [{label}]({url})\n")).is_empty());
        // Table cells are not paragraphs, so the spaces between two URLs are not prose either.
        assert!(reports(&format!("| a | {url} | {url} |\n| - | - | - |\n")).is_empty());

        // The same row with the URL spelled as prose, and nothing spares it.
        assert_eq!(
            vec![1],
            reports(&format!("| a | {} |\n| - | - |\n", url.replace('/', " ")))
        );
        // In a paragraph the space between two URLs is the paragraph's own text.
        assert_eq!(vec![1], reports(&format!("{url} {url}\n")));

        // An address inside a URL whose scheme GFM does not autolink is still a `literalAutolink` to
        // micromark. linkify swallows it into the URL, so it has to be found separately — and folding
        // does not spare this row, whose last run of non-whitespace is the closing pipe.
        let credential = "oss://LTAI5tKUdt5adv4sdhfwQr7y:\
                          ynFqbeSNumdWyJerfTa5I2a1DsfXZS@oss-cn-shanghai.aliyuncs.com/bak";
        assert!(
            reports(&format!("| a | b |\n|---|---|\n| {credential} | x |\n")).is_empty(),
            "a 102-column row"
        );

        // A task item's `[x] ` is two `data` tokens of the paragraph to micromark, which has no task
        // marker token at all, so the line holds prose and folding is the only thing that could spare
        // it — and the label's own spaces are past the limit, so it does not.
        assert_eq!(
            vec![1],
            reports("  - [x] [to #61124615, make KEPLER_CSTORE_PARTITION_XIHE_MERGED show \
                     \"LOCAL\" for LOCAL_SYNC](https://code.alibaba-inc.com/garuda/adb/codereview/19220742)\n")
        );
    }

    /// markdownlint strips front matter from the content before it parses, so no rule measures those
    /// lines. Only a matched `---` or `+++` pair counts: markdownlint-cli2 does not accept `...` as a
    /// closing delimiter. Every expectation is a markdownlint-cli2 v0.23.3 measurement.
    #[test]
    fn front_matter_lines_are_not_measured() {
        fn reported(input: &str) -> Vec<usize> {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
            linter
                .analyze()
                .iter()
                .map(|violation| violation.location().range.start.line + 1)
                .collect()
        }

        let long = "x ".repeat(60);
        let body = format!("# t\n\n{long}\n");
        for delimiters in ["---", "+++"] {
            let input = format!("{delimiters}\ndescription: {long}\n{delimiters}\n\n{body}");
            assert_eq!(vec![7], reported(&input), "{delimiters}");
        }
        // Neither `...` nor running out of document closes front matter, so in both cases the long
        // `description:` line is measured like any other.
        assert_eq!(
            vec![2, 7],
            reported(&format!("---\ndescription: {long}\n...\n\n{body}"))
        );
        assert_eq!(
            vec![2, 6],
            reported(&format!("---\ndescription: {long}\n\n{body}"))
        );
    }
    /// Outside strict and stern mode markdownlint compares the line with its last run of
    /// non-whitespace folded to a single character, and it measures every length in UTF-16 code
    /// units. Every expectation is a markdownlint-cli2 v0.23.3 measurement at `line_length = 20`.
    #[test]
    fn a_long_last_word_is_folded_before_the_limit_is_applied() {
        let table = MD013LineLengthTable {
            line_length: 20,
            ..MD013LineLengthTable::default()
        };
        let cases: Vec<(String, Option<usize>)> = vec![
            // Folds to `aaa bbb ccc ddd eee #`, which is still over the limit.
            ("aaa bbb ccc ddd eee fff\n".to_string(), Some(23)),
            ("aaa bbb ccc ddd eee ff\n".to_string(), Some(22)),
            // Folds to 21, but the line's own length is exactly the limit and `addErrorDetailIf`
            // stays quiet when expected equals actual.
            ("aaa bbb ccc ddd eee \n".to_string(), None),
            ("aaaaaaaaaaaaaaaaaaaa bbb\n".to_string(), Some(24)),
            // One unbreakable run folds to `#`, so there is nowhere to wrap and nothing to report.
            ("aaaaaaaaaaaaaaaaaaaabbbbb\n".to_string(), None),
            // 32 UTF-16 units in 80 bytes; what gets reported is the units.
            (("日本語 ".repeat(8)) + "\n", Some(32)),
            // An astral character is two units, so 45 units in 75 bytes.
            (("😀 ".repeat(15)) + "\n", Some(45)),
        ];

        for (input, actual) in cases {
            let config = test_config_with_line_length(table.clone());
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, &input);
            let violations = linter.analyze();
            match actual {
                None => assert!(violations.is_empty(), "{input:?}"),
                Some(actual) => {
                    assert_eq!(1, violations.len(), "{input:?}");
                    assert!(
                        violations[0]
                            .message()
                            .contains(&format!("Expected: 20; Actual: {actual}")),
                        "{input:?}: {}",
                        violations[0].message()
                    );
                }
            }
        }
    }

    #[test]
    fn test_headings_disabled() {
        let line_length_config = MD013LineLengthTable {
            headings: false,
            ..MD013LineLengthTable::default()
        };

        let input = "# This is a very long heading that definitely exceeds the eighty character limit and should not trigger a violation";

        let config = test_config_with_line_length(line_length_config);
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(0, violations.len());
    }

    /// Everything below is a markdownlint-cli2 0.23.3 measurement at `line_length = 20`, one config
    /// per assertion; the expected lines are `range.start.line`, which is 0-based, so they are one
    /// less than the line markdownlint prints.
    ///
    /// `headings` and `tables` were inert until MD013 stopped asking the node cache which kind a line
    /// belonged to. That lookup picked the smallest covering node, and on a heading or table line
    /// several nodes tie, so `HashMap` iteration order decided the answer and the same document could
    /// lint differently in two processes.
    #[test]
    fn test_headings_disabled_covers_every_heading_shape() {
        fn lines(input: &str, headings: bool) -> Vec<usize> {
            let config = test_config_with_line_length(MD013LineLengthTable {
                line_length: 20,
                heading_line_length: Some(20),
                code_block_line_length: Some(20),
                headings,
                ..MD013LineLengthTable::default()
            });
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            linter
                .analyze()
                .iter()
                .map(|v| v.location().range.start.line)
                .collect()
        }

        // `## …` was never exempt before: the text heuristic only recognised `# `. The exemption has
        // to stop at the heading's last line, so the paragraph on line 4 is still reported.
        let atx = "# H\n\n## a heading whose last word is way out here\n\
                   a paragraph line whose last word is way out here\n";
        assert_eq!(lines(atx, true), vec![2, 3]);
        assert_eq!(lines(atx, false), vec![3]);

        // A setext heading spans two lines; neither is reported once headings are off.
        let setext = "# H\n\na setext heading whose last word is out here\n\
                      =============================================\n";
        assert_eq!(lines(setext, true), vec![2]);
        assert_eq!(lines(setext, false), Vec::<usize>::new());

        // A heading at EOF has no trailing newline, so its end row *is* its last content row.
        let eof = "# H\n\n## a heading whose last word is way out here";
        assert_eq!(lines(eof, true), vec![2]);
        assert_eq!(lines(eof, false), Vec::<usize>::new());
    }

    #[test]
    fn test_tables_disabled() {
        fn lines(input: &str, tables: bool) -> Vec<usize> {
            let config = test_config_with_line_length(MD013LineLengthTable {
                line_length: 20,
                heading_line_length: Some(20),
                code_block_line_length: Some(20),
                tables,
                ..MD013LineLengthTable::default()
            });
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            linter
                .analyze()
                .iter()
                .map(|v| v.location().range.start.line)
                .collect()
        }

        let input = "# H\n\n| a | b |\n|---|---|\n| xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx | y |\n\n\
                     xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx paragraph after table\n";
        assert_eq!(lines(input, true), vec![4, 6]);
        assert_eq!(lines(input, false), vec![6]);
    }

    #[test]
    fn test_code_blocks_disabled_covers_both_code_shapes() {
        fn lines(input: &str, code_blocks: bool) -> Vec<usize> {
            let config = test_config_with_line_length(MD013LineLengthTable {
                line_length: 20,
                heading_line_length: Some(20),
                code_block_line_length: Some(20),
                code_blocks,
                ..MD013LineLengthTable::default()
            });
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            linter
                .analyze()
                .iter()
                .map(|v| v.location().range.start.line)
                .collect()
        }

        let fenced = "# H\n\n```text\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx inside a fence here\n```\n\
                      xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx paragraph right after\n";
        assert_eq!(lines(fenced, true), vec![3, 5]);
        assert_eq!(lines(fenced, false), vec![5]);

        let indented = "# H\n\n    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx indented code here\n\n\
                        xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx paragraph after code\n";
        assert_eq!(lines(indented, true), vec![2, 4]);
        assert_eq!(lines(indented, false), vec![4]);
    }

    #[test]
    fn test_multiple_lines() {
        let input = "This is a short line.
This is a very long line that definitely exceeds the eighty character limit and should trigger a violation.
Another short line.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(1, violations.len());
    }

    #[test]
    fn test_demonstrates_potential_bug_scenario() {
        // This test demonstrates that our concern was valid in theory, but doesn't occur in practice
        // because the parser creates enough AST nodes for even simple documents

        let input = "A\nB\nC\n"; // Minimal document - just 3 short lines

        let node_count = crate::ast::build::parse(input).node_count();

        println!("Even a 3-line minimal document creates {node_count} AST nodes");
        println!("This explains why our MD013 implementation works correctly");

        // Even this tiny document creates multiple nodes (document, paragraph, text nodes, etc.)
        assert!(
            node_count >= 3,
            "Even minimal documents create multiple AST nodes"
        );
    }

    #[test]
    fn test_extreme_violations_vs_minimal_nodes() {
        // Create the most minimal AST possible: just plain text with no structure
        // This should create minimal AST nodes but many violations
        let mut input = String::new();

        // Add 100 long lines of plain text (no markdown structure at all)
        let long_line = "This line is definitely longer than 80 characters and should trigger a line length violation every single time.\n";
        assert!(
            long_line.len() > 80,
            "Test line should exceed 80 chars, got {}",
            long_line.len()
        );

        for i in 0..100 {
            input.push_str(&format!("Violation line {}: {}", i + 1, long_line));
        }

        println!("Total input length: {} chars", input.len());
        println!("Number of lines: {}", input.lines().count());

        // Count how many AST nodes are created by parsing this document
        let node_count = crate::ast::build::parse(&input).node_count();
        println!("Total AST nodes: {node_count}");

        let config = test_config();
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, &input);
        let violations = linter.analyze();

        println!("Violations found: {}", violations.len());

        // This is the critical test: with the improved MD013, we should ALWAYS find all violations
        // regardless of the node count, because violations are tied to line numbers, not node traversal order
        println!(
            "Ratio: {} violations vs {} nodes",
            violations.len(),
            node_count
        );

        // We should find exactly 100 violations
        assert_eq!(100, violations.len(),
            "Expected 100 line length violations but found {}. The improved MD013 should never lose violations!",
            violations.len()
        );
    }

    #[test]
    fn test_violation_node_mismatch_scenario() {
        // This test creates a scenario where violations > nodes to ensure our fix works
        // Create a document with minimal structure but maximum line violations

        let mut input = "# Header\n\n".to_string(); // Creates multiple AST nodes

        // Add 50 long lines that should violate but may not have corresponding unique AST nodes
        for i in 0..50 {
            input.push_str(&format!("Line {} with text that is definitely over eighty characters and should trigger MD013 violation\n", i + 1));
        }

        let node_count = crate::ast::build::parse(&input).node_count();

        let config = test_config();
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, &input);
        let violations = linter.analyze();

        println!(
            "Stress test: {} violations vs {} nodes",
            violations.len(),
            node_count
        );

        // Should find exactly 50 violations (one per long line), regardless of node count
        assert_eq!(
            50,
            violations.len(),
            "Expected 50 violations but found {}. Improved MD013 must not lose violations!",
            violations.len()
        );

        // Verify each violation is on the correct line
        for (i, violation) in violations.iter().enumerate() {
            let expected_line = i + 2; // Lines 2, 3, 4, ..., 51 (line 0 is header, line 1 is empty)
            assert_eq!(
                expected_line,
                violation.location().range.start.line,
                "Violation {} should be on line {} but was on line {}",
                i + 1,
                expected_line,
                violation.location().range.start.line
            );
        }
    }

    #[test]
    fn test_many_violations_vs_few_nodes() {
        // Create a document with many line violations but few AST nodes
        // Structure: simple heading followed by many long lines of plain text
        let mut input = "# Short heading\n\n".to_string();

        // Add 20 long lines that should each trigger violations
        let long_line = "This line is definitely longer than 80 characters and should trigger a line length violation every time it appears.\n";
        assert!(
            long_line.len() > 80,
            "Test line should exceed 80 chars, got {}",
            long_line.len()
        );

        for i in 0..20 {
            input.push_str(&format!("Line {}: {}", i + 1, long_line));
        }

        println!("Total input length: {} chars", input.len());
        println!("Number of lines: {}", input.lines().count());

        let config = test_config();
        let mut linter =
            MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, &input);
        let violations = linter.analyze();

        // Debug: print actual violations found
        println!("Violations found: {}", violations.len());
        for (i, violation) in violations.iter().enumerate() {
            println!(
                "  Violation {}: line {}",
                i + 1,
                violation.location().range.start.line
            );
        }

        // We should find exactly 20 violations (one per long line)
        // If we find fewer, it means some violations were lost due to the bug
        assert_eq!(20, violations.len(),
            "Expected 20 line length violations but found {}. This suggests violations were lost due to insufficient AST nodes.",
            violations.len()
        );

        // Verify violations are on the correct lines (lines 2-21, since line 0 is heading, line 1 is empty)
        for (i, violation) in violations.iter().enumerate() {
            let expected_line = i + 2; // Lines 2, 3, 4, ..., 21
            assert_eq!(
                expected_line,
                violation.location().range.start.line,
                "Violation {} should be on line {} but was on line {}",
                i + 1,
                expected_line,
                violation.location().range.start.line
            );
        }
    }

    #[test]
    fn test_utf8_character_boundary_fix() {
        // Test that UTF-8 character boundary issues are properly handled
        // Create a line that has a multi-byte UTF-8 character at position 79-82 (checkmark ✓)
        // This previously caused a panic when slicing at position 80
        let input = "| View allowed and denied licenses **(ULTIMATE)** | ✓ (*1*) | ✓          | ✓           | ✓        | ✓      |";

        // Verify the test setup: checkmark should be at the boundary where slicing fails
        assert!(input.len() > 80, "Line should exceed 80 characters");
        let char_at_79 = input.as_bytes()[79];
        // UTF-8 checkmark starts at byte 79, so slicing at 80 would panic without the fix
        assert!(
            char_at_79 >= 0x80,
            "Should have multi-byte UTF-8 character near position 80"
        );

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        // This should NOT panic with the UTF-8 boundary fix
        let violations = linter.analyze();

        // Should find exactly 1 violation for the long line
        assert_eq!(1, violations.len(), "Should find one line length violation");
        assert_eq!("MD013", violations[0].rule().id);
        assert!(violations[0].message().contains("Expected: 80"));
    }
}
