use std::rc::Rc;

use crate::ast::Node;
use linkify::{LinkFinder, LinkKind};

use crate::{
    linter::{range_from_node_range, RuleViolation},
    rules::{Context, Rule, RuleLinter, RuleType},
};

pub(crate) struct MD034Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
    finder: LinkFinder,
}

impl MD034Linter {
    pub fn new(context: Rc<Context>) -> Self {
        // linkify only looks for scheme-less domains when told to, and GFM autolinks `www.example.com`
        // without a scheme. Everything else it would find that way — `example.com`, `foo.bar` — is
        // literal text to micromark, so `is_gfm_autolink` filters it back out.
        let mut finder = LinkFinder::new();
        finder.url_must_have_scheme(false);
        Self {
            context,
            violations: Vec::new(),
            finder,
        }
    }
}

/// Whether `url` starts with one of the schemes GFM's autolink-literal extension recognises. Those,
/// plus email addresses, are the only things micromark turns into a `literalAutolink` token, so
/// `ftp://`, `file://`, `git://` and `oss://` are literal text and MD034 must leave them alone.
fn is_gfm_autolink(url: &str) -> bool {
    ["http://", "https://", "www."].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    })
}

impl RuleLinter for MD034Linter {
    fn feed(&mut self, node: &Node) {
        // `inline`, not `paragraph`: micromark's `literalAutolink` tokens live in headings and table
        // cells too, and matching only paragraphs left every bare URL in a table unreported.
        if node.kind() == "inline" {
            let content = self.context.document_content.borrow();
            let text = node.utf8_text(content.as_bytes()).unwrap_or("").to_string();
            let node_range = node.range();
            drop(content); // Release the borrow before calling mutable methods

            self.check_for_bare_urls_in_text(&text, &node_range);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

impl MD034Linter {
    fn check_for_bare_urls_in_text(&mut self, text: &str, paragraph_range: &crate::ast::NodeRange) {
        for link in self.finder.links(text) {
            let link_start = link.start();
            let link_end = link.end();
            let link_text = link.as_str();

            // Skip if this link is already properly formatted
            if !self.is_link_properly_formatted(text, link_start, link_text, link.kind()) {
                // The URL's own bytes, not an offset from the paragraph's first line: a bare URL on
                // a wrapped continuation line belongs to that line, and adding a byte offset to the
                // paragraph's column put every such URL on the paragraph's first line instead.
                let start_byte = paragraph_range.start_byte + link_start;
                let end_byte = paragraph_range.start_byte + link_end;
                let violation_range = crate::ast::NodeRange {
                    start_byte,
                    end_byte,
                    start_point: self.context.point_at(start_byte),
                    end_point: self.context.point_at(end_byte),
                };

                self.violations.push(RuleViolation::new(
                    &MD034,
                    format!("{} [Context: \"{}\"]", MD034.description, link_text),
                    self.context.file_path.clone(),
                    range_from_node_range(&violation_range),
                ));
            }
        }
    }

    fn is_link_properly_formatted(
        &self,
        text: &str,
        link_start: usize,
        link_text: &str,
        link_kind: &LinkKind,
    ) -> bool {
        match link_kind {
            // linkify recognises far more schemes than GFM's autolink-literal extension does, and
            // only an autolinked URL is a bare one as far as markdownlint is concerned.
            LinkKind::Url => {
                !is_gfm_autolink(link_text)
                    || self.is_url_properly_formatted(text, link_start, link_text)
            }
            LinkKind::Email => self.is_email_properly_formatted(text, link_start, link_text),
            _ => true, // Other link types are not handled by MD034
        }
    }

    fn is_url_properly_formatted(&self, text: &str, url_start: usize, url_text: &str) -> bool {
        // Check if linkify included backticks in the URL (this happens with code spans)
        if url_text.starts_with('`') {
            // This URL is inside a code span according to linkify
            return true;
        }

        // Check if URL is in angle brackets: <https://example.com>. `url_start` is a byte offset, so
        // the neighbours have to be read as bytes — indexing characters with it looked one character
        // too far left for every non-ASCII byte upstream and missed the bracket.
        let bytes = text.as_bytes();
        if url_start > 0
            && bytes[url_start - 1] == b'<'
            && bytes.get(url_start + url_text.len()) == Some(&b'>')
        {
            return true;
        }

        // Check if URL is in markdown link: [text](https://example.com)
        if let Some(link_start) = text[..url_start].rfind("](") {
            if url_start == link_start + 2 {
                return true; // URL is right after ](
            }
            // Also check if URL is after ]( with some prefix (like mailto:, ftp:, etc.)
            let after_paren = link_start + 2;
            let prefix_text = &text[after_paren..url_start];
            if prefix_text.chars().all(|c| c.is_alphabetic() || c == ':') {
                return true; // URL is in markdown link target with scheme prefix
            }
        }

        // Check if URL is in markdown link text: [text with https://example.com](target)
        if let Some(bracket_start) = text[..url_start].rfind('[') {
            // Look for closing bracket and opening paren after the URL
            let url_end = url_start + url_text.len();
            if let Some(_bracket_end) = text[url_end..].find("](") {
                // Check that there's no unmatched bracket between bracket_start and url_start
                let link_text = &text[bracket_start + 1..url_start];
                if !link_text.contains('[') && !link_text.contains(']') {
                    return true; // URL is in link text
                }
            }
        }

        // Check if URL is in HTML tag attribute
        if let Some(attr_start) = text[..url_start].rfind("href=\"") {
            if url_start == attr_start + 6 {
                return true;
            }
        }
        if let Some(attr_start) = text[..url_start].rfind("href='") {
            if url_start == attr_start + 6 {
                return true;
            }
        }

        // Check if URL is in code span using backtick counting
        let before_url = &text[..url_start];
        let after_url = &text[url_start + url_text.len()..];

        let backticks_before = before_url.matches('`').count();
        if backticks_before % 2 == 1 {
            // Odd number of backticks before means we're likely inside a code span
            // Check if there's a closing backtick after the URL
            if after_url.contains('`') {
                return true;
            }
        }

        false
    }

    fn is_email_properly_formatted(
        &self,
        text: &str,
        email_start: usize,
        email_text: &str,
    ) -> bool {
        // Check if linkify included backticks in the email (this happens with code spans)
        if email_text.starts_with('`') {
            // This email is inside a code span according to linkify
            return true;
        }

        // Check if email is in markdown link: [text](mailto:user@example.com)
        if let Some(link_start) = text[..email_start].rfind("](") {
            // Check if email is right after ]( or after ]( with prefix like mailto:
            let after_paren = link_start + 2;
            if email_start == after_paren {
                return true; // Email is right after ](
            }
            let prefix_text = &text[after_paren..email_start];
            if prefix_text.chars().all(|c| c.is_alphabetic() || c == ':') {
                return true; // Email is in markdown link target with scheme prefix
            }
        }

        // Check if email is in angle brackets: <user@example.com> or <mailto:user@example.com>.
        // Byte offsets index bytes; see the same note in `is_url_properly_formatted`.
        let bytes = text.as_bytes();
        let mut check_start = email_start;

        // Look backward for opening angle bracket, potentially with "mailto:" prefix
        while check_start > 0 {
            let byte_before = bytes[check_start - 1];
            if byte_before == b'<' {
                if bytes.get(email_start + email_text.len()) == Some(&b'>') {
                    return true;
                }
                break;
            } else if byte_before.is_ascii_alphabetic() || byte_before == b':' {
                // Continue looking backward through "mailto:" prefix
                check_start -= 1;
            } else {
                break;
            }
        }

        // Check if email is in markdown link text: [text with user@example.com](target)
        if let Some(bracket_start) = text[..email_start].rfind('[') {
            // Look for closing bracket and opening paren after the email
            let email_end = email_start + email_text.len();
            if let Some(_bracket_end) = text[email_end..].find("](") {
                // Check that there's no unmatched bracket between bracket_start and email_start
                let link_text = &text[bracket_start + 1..email_start];
                if !link_text.contains('[') && !link_text.contains(']') {
                    return true; // Email is in link text
                }
            }
        }

        // Check if email is in code span using backtick counting
        let before_email = &text[..email_start];
        let after_email = &text[email_start + email_text.len()..];

        // Count backticks before email to see if we're inside a code span
        let backticks_before = before_email.matches('`').count();
        if backticks_before % 2 == 1 {
            // Odd number of backticks before means we're likely inside a code span
            // Check if there's a closing backtick after the email
            if after_email.contains('`') {
                return true;
            }
        }

        false
    }
}

pub const MD034: Rule = Rule {
    id: "MD034",
    alias: "no-bare-urls",
    tags: &["links", "url"],
    description: "Bare URL used",
    rule_type: RuleType::Token,
    required_nodes: &["inline"],
    new_linter: |context| Box::new(MD034Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::RuleSeverity;
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![
            ("no-bare-urls", RuleSeverity::Error),
            ("heading-increment", RuleSeverity::Off),
            ("heading-style", RuleSeverity::Off),
            ("line-length", RuleSeverity::Off),
        ])
    }

    #[test]
    fn test_bare_url_detection() {
        let input = "Visit https://example.com for more info.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // This test should fail initially, then pass once we implement the logic properly
        assert_eq!(1, violations.len());
        let violation = &violations[0];
        assert_eq!("MD034", violation.rule().id);
        assert!(violation.message().contains("Bare URL used"));
        assert!(violation.message().contains("https://example.com"));
    }

    #[test]
    fn test_bare_email_detection() {
        let input = "Email me at user@example.com for questions.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(1, violations.len());
        let violation = &violations[0];
        assert_eq!("MD034", violation.rule().id);
        assert!(violation.message().contains("user@example.com"));
    }

    #[test]
    fn test_angle_bracket_urls_no_violation() {
        let input = "Visit <https://example.com> for more info.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should not trigger violation for properly formatted URLs
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_angle_bracket_emails_no_violation() {
        let input = "Email me at <user@example.com> for questions.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_code_span_urls_no_violation() {
        let input = "Not a link: `https://example.com`";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // URLs in code spans should not trigger violations
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_markdown_link_urls_no_violation() {
        let input = "Visit [the site](https://example.com) for more info.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // URLs in proper markdown links should not trigger violations
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_html_tag_urls_no_violation() {
        let input = "<a href='https://example.com'>Link text</a>";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // URLs inside HTML tags should not trigger violations
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_multiple_bare_urls() {
        let input = "Visit https://first.com and https://second.com and email admin@site.com";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should detect all three bare URLs/emails
        assert_eq!(3, violations.len());
    }

    #[test]
    fn test_mixed_urls_and_proper_links() {
        let input = "Visit https://bare.com and [proper link](https://proper.com) and <https://formatted.com>";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should only detect the bare URL, not the properly formatted ones
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("https://bare.com"));
    }

    #[test]
    fn test_mailto_urls_in_markdown_links_no_violation() {
        let input = "Email [support](mailto:user@example.com) for help.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should not trigger violation for emails in mailto: markdown links
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_urls_in_markdown_link_text_no_violation() {
        let input = "[link text with https://example.com in it](https://proper-target.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should not trigger violation for URLs in markdown link text
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_emails_in_markdown_link_text_no_violation() {
        let input = "[contact user@example.com for support](https://contact-form.com)";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should not trigger violation for emails in markdown link text
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_scheme_prefixes_in_markdown_links_no_violation() {
        let input = "Try [FTP site](ftp://files.example.com) and [secure site](https://secure.example.com).";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should not trigger violations for URLs with various schemes in markdown links
        assert_eq!(0, violations.len());
    }

    #[test]
    fn test_nested_markdown_scenarios() {
        let input = "Links bind to the innermost [link that https://example.com link](https://target.com) but https://bare.com should trigger.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should only detect the bare URL, not the one in link text
        assert_eq!(1, violations.len());
        assert!(violations[0].message().contains("https://bare.com"));
    }

    #[test]
    fn test_complex_mixed_scenarios() {
        let input = r#"
Visit https://bare.com for info.
Email [support](mailto:help@example.com) or bare.email@example.com.
Check [site with https://url-in-text.com info](https://real-target.com).
Use <https://angle-bracketed.com> or `https://code-span.com`.
"#;

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should detect:
        // 1. https://bare.com (bare URL)
        // 2. bare.email@example.com (bare email)
        // Should NOT detect:
        // - help@example.com (in mailto: link)
        // - https://url-in-text.com (in link text)
        // - https://real-target.com (in link target)
        // - https://angle-bracketed.com (in angle brackets)
        // - https://code-span.com (in code span)
        assert_eq!(2, violations.len());

        let violation_contexts: Vec<String> = violations
            .iter()
            .map(|v| {
                // Extract the context from the message
                let msg = v.message();
                let start = msg.find("[Context: \"").unwrap() + 11;
                let end = msg.find("\"]").unwrap();
                msg[start..end].to_string()
            })
            .collect();

        assert!(violation_contexts.contains(&"https://bare.com".to_string()));
        assert!(violation_contexts.contains(&"bare.email@example.com".to_string()));
    }

    #[test]
    fn test_international_domains_and_emails() {
        let input = "Visit https://müller.example and email ünser@müller.example for info.";

        let config = test_config();
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();

        // Should detect both international URL and email
        assert_eq!(2, violations.len());

        let violation_contexts: Vec<String> = violations
            .iter()
            .map(|v| {
                let msg = v.message();
                let start = msg.find("[Context: \"").unwrap() + 11;
                let end = msg.find("\"]").unwrap();
                msg[start..end].to_string()
            })
            .collect();

        assert!(violation_contexts.contains(&"https://müller.example".to_string()));
        assert!(violation_contexts.contains(&"ünser@müller.example".to_string()));
    }

    /// A wrapped paragraph is one node spanning several lines, so a URL's position has to come from
    /// its own bytes. Attributing it to the node's first line put every wrapped URL on the wrong
    /// line — measured, that was 353 misplaced reports on the vault corpus.
    ///
    /// Every expectation is a markdownlint-cli2 0.23.3 measurement, converted to 0-based.
    #[test]
    fn test_reports_the_line_the_url_is_on() {
        fn positions(input: &str) -> Vec<(usize, usize)> {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
            linter
                .analyze()
                .iter()
                .filter(|v| v.rule().id == "MD034")
                .map(|v| {
                    (
                        v.location().range.start.line,
                        v.location().range.start.character,
                    )
                })
                .collect()
        }

        assert_eq!(
            vec![(0, 4), (1, 4)],
            positions("one https://a.example/x\ntwo https://b.example/y\n")
        );
        assert_eq!(
            vec![(1, 2)],
            positions("text before\n  https://example.com/a\n")
        );
        assert_eq!(
            vec![(1, 2)],
            positions("> quoted\n> https://example.com/a\n")
        );
        assert_eq!(
            vec![(1, 9)],
            positions("para one\npara two https://example.com/a\npara three\n")
        );
    }

    /// markdownlint reports a bare URL wherever micromark makes a `literalAutolink` token, which is
    /// anywhere inline: a heading and a table cell as much as a paragraph. All measured.
    #[test]
    fn test_finds_bare_urls_in_headings_and_table_cells() {
        fn count(input: &str) -> usize {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
            linter
                .analyze()
                .iter()
                .filter(|v| v.rule().id == "MD034")
                .count()
        }

        assert_eq!(1, count("## https://example.com/a\n"));
        assert_eq!(
            1,
            count("| a | b |\n|---|---|\n| x | https://example.com/a |\n")
        );
        // A proper link in either place is not bare.
        assert_eq!(0, count("## [text](https://example.com/a)\n"));
        assert_eq!(
            0,
            count("| a | b |\n|---|---|\n| x | [t](https://example.com/a) |\n")
        );
    }

    /// GFM's autolink-literal extension recognises `http://`, `https://`, `www.` and email
    /// addresses, and nothing else. linkify finds far more schemes, so the rest have to be filtered
    /// out — micromark leaves `oss://bucket/object` as literal text and markdownlint never reports
    /// it. All measured.
    #[test]
    fn test_only_gfm_schemes_are_bare_urls() {
        fn count(input: &str) -> usize {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
            linter
                .analyze()
                .iter()
                .filter(|v| v.rule().id == "MD034")
                .count()
        }

        assert_eq!(1, count("http://example.com/a\n"));
        assert_eq!(1, count("HTTP://EXAMPLE.COM/a\n"));
        assert_eq!(1, count("www.example.com/a\n"));
        assert_eq!(1, count("someone@example.com\n"));
        assert_eq!(0, count("ftp://example.com/a\n"));
        assert_eq!(0, count("file:///tmp/x\n"));
        assert_eq!(0, count("git://example.com/a\n"));
        assert_eq!(0, count("oss://bucket/object.tar.gz\n"));
        // Only the https one of the two is bare.
        assert_eq!(1, count("oss://b/o and https://ok.example/a\n"));
    }

    /// The angle-bracket test reads the neighbours of a byte offset, so it has to index bytes. With
    /// character indexing, every non-ASCII byte upstream shifted the test off the bracket and an
    /// autolink after CJK text was reported as bare. Measured.
    #[test]
    fn test_angle_brackets_after_multibyte_text_are_not_bare() {
        fn count(input: &str) -> usize {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), test_config(), input);
            linter
                .analyze()
                .iter()
                .filter(|v| v.rule().id == "MD034")
                .count()
        }

        assert_eq!(0, count("POC参考文档:<https://example.com/a> 相比ADB\n"));
        assert_eq!(0, count("café <https://example.com/a>\n"));
        assert_eq!(0, count("<https://example.com/a>\n"));
        // The same line without the brackets is bare.
        assert_eq!(1, count("see 中文 https://example.com/a here\n"));
    }
}
