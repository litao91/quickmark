use crate::ast::Node;
use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;
use unicode_width::UnicodeWidthStr;

use crate::linter::{range_from_node_range, Context, RuleLinter, RuleViolation};

use super::{Rule, RuleType};

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub enum TableColumnStyle {
    #[serde(rename = "aligned")]
    Aligned,
    #[serde(rename = "any")]
    Any,
    #[serde(rename = "compact")]
    Compact,
    #[serde(rename = "tight")]
    Tight,
}

impl Default for TableColumnStyle {
    fn default() -> Self {
        Self::Any
    }
}

#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct MD060TableColumnStyleTable {
    #[serde(default)]
    pub style: TableColumnStyle,
    #[serde(default)]
    pub aligned_delimiter: bool,
}

impl Default for MD060TableColumnStyleTable {
    fn default() -> Self {
        Self {
            style: TableColumnStyle::Any,
            aligned_delimiter: false,
        }
    }
}

/// A pipe that does not line up with the header, or that has the wrong amount of space around it.
/// The node is kept so the violation can be reported at the pipe itself.
#[derive(Clone)]
struct PipeError<'a> {
    node: Node<'a>,
    message: &'static str,
}

/// The display width of `text`, counted the way markdownlint's `string-width` counts it.
///
/// `unicode-width` gives a control character one column where `string-width` gives it none, and
/// that is the only difference this has to correct: one stray C0 byte in a cell would otherwise
/// shift every pipe after it and a row that is not aligned would look as if it were. Splitting
/// rather than filtering keeps the measurement over contiguous text, so an emoji sequence still
/// counts as one wide glyph.
///
/// `string-width` also strips ANSI escape sequences, which this does not — a cell holding one is
/// measured five columns too wide for `\u{1b}[31m`.
fn display_width(text: &str) -> usize {
    text.split(|ch: char| ch.is_control())
        .map(UnicodeWidthStr::width)
        .sum()
}

pub(crate) struct MD060Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD060Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// The pipe characters of a row, in order.
    fn dividers<'a>(row: &Node<'a>) -> Vec<Node<'a>> {
        let mut cursor = row.walk();
        row.children(&mut cursor)
            .filter(|child| child.kind() == "|")
            .collect()
    }

    /// Column of a pipe measured in terminal display columns, so that wide (for example CJK)
    /// glyphs count double and two rows that look aligned really compare equal. This mirrors
    /// markdownlint, which uses `string-width` for the same reason.
    fn effective_column(&self, line: &str, byte_column: usize) -> usize {
        line.get(..byte_column).map_or(0, display_width)
    }

    /// Every pipe after the header row that does not sit at one of the header's columns.
    fn check_aligned<'a>(
        &self,
        lines: &[String],
        rows: &[Node<'a>],
        message: &'static str,
    ) -> Vec<PipeError<'a>> {
        let mut errors = Vec::new();
        let Some(header) = rows.first() else {
            return errors;
        };
        let header_line = &lines[header.start_position().row];
        let header_columns: Vec<usize> = Self::dividers(header)
            .iter()
            .map(|pipe| self.effective_column(header_line, pipe.start_position().column))
            .collect();

        for row in rows.iter().skip(1) {
            let Some(line) = lines.get(row.start_position().row) else {
                continue;
            };
            // Consumed as it goes, so a row with a repeated column still reports the second one.
            let mut remaining: HashSet<usize> = header_columns.iter().copied().collect();
            for pipe in Self::dividers(row) {
                let column = self.effective_column(line, pipe.start_position().column);
                if !remaining.is_empty() && !remaining.remove(&column) {
                    errors.push(PipeError {
                        node: pipe,
                        message,
                    });
                }
            }
        }

        errors
    }

    /// Space around each pipe of each row, judged against the "compact" (exactly one space) and
    /// "tight" (no space) ideals. A pipe at the start or end of a row has nothing on that side.
    fn check_spacing<'a>(
        lines: &[String],
        rows: &[Node<'a>],
    ) -> (Vec<PipeError<'a>>, Vec<PipeError<'a>>) {
        let mut compact = Vec::new();
        let mut tight = Vec::new();

        for row in rows {
            let Some(line) = lines.get(row.start_position().row) else {
                continue;
            };
            let bytes = line.as_bytes();
            let row_start = row.start_position().column;
            let row_end = row.end_position().column;

            for pipe in Self::dividers(row) {
                let column = pipe.start_position().column;

                if column > row_start {
                    let mut left = 0;
                    while column - left > row_start && bytes.get(column - left - 1) == Some(&b' ') {
                        left += 1;
                    }
                    if left == 0 {
                        compact.push(PipeError {
                            node: pipe,
                            message:
                                "Table pipe is missing space to the left for style \"compact\"",
                        });
                    } else {
                        if left > 1 {
                            compact.push(PipeError {
                                node: pipe,
                                message:
                                    "Table pipe has extra space to the left for style \"compact\"",
                            });
                        }
                        tight.push(PipeError {
                            node: pipe,
                            message: "Table pipe has space to the left for style \"tight\"",
                        });
                    }
                }

                let mut right = 0;
                while bytes.get(column + 1 + right) == Some(&b' ') {
                    right += 1;
                }
                // Whitespace that runs to the end of the row is trailing padding, not a separator.
                if column + 1 + right < row_end {
                    if right == 0 {
                        compact.push(PipeError {
                            node: pipe,
                            message:
                                "Table pipe is missing space to the right for style \"compact\"",
                        });
                    } else {
                        if right > 1 {
                            compact.push(PipeError {
                                node: pipe,
                                message:
                                    "Table pipe has extra space to the right for style \"compact\"",
                            });
                        }
                        tight.push(PipeError {
                            node: pipe,
                            message: "Table pipe has space to the right for style \"tight\"",
                        });
                    }
                }
            }
        }

        (compact, tight)
    }

    fn check_table(&mut self, table: &Node) {
        let config = &self.context.config.linters.settings.table_column_style;
        let aligned_allowed = matches!(
            config.style,
            TableColumnStyle::Any | TableColumnStyle::Aligned
        );
        let compact_allowed = matches!(
            config.style,
            TableColumnStyle::Any | TableColumnStyle::Compact
        );
        let tight_allowed = matches!(
            config.style,
            TableColumnStyle::Any | TableColumnStyle::Tight
        );

        let mut cursor = table.walk();
        let children: Vec<Node> = table.children(&mut cursor).collect();

        let rows: Vec<Node> = children
            .into_iter()
            .filter(|child| {
                matches!(
                    child.kind(),
                    "pipe_table_header" | "pipe_table_delimiter_row" | "pipe_table_row"
                )
            })
            .collect();
        if rows.is_empty() {
            return;
        }

        let lines = self.context.lines.borrow();

        let errors_if_aligned = if aligned_allowed {
            self.check_aligned(
                &lines,
                &rows,
                "Table pipe does not align with header for style \"aligned\"",
            )
        } else {
            Vec::new()
        };

        let (mut errors_if_compact, mut errors_if_tight) = if (compact_allowed || tight_allowed)
            && !(aligned_allowed && errors_if_aligned.is_empty())
        {
            let spacing = Self::check_spacing(&lines, &rows);
            if config.aligned_delimiter {
                let delimiter = self.check_aligned(
                    &lines,
                    &rows[..rows.len().min(2)],
                    "Table pipe does not align with header for option \"aligned_delimiter\"",
                );
                (
                    [delimiter.clone(), spacing.0].concat(),
                    [delimiter, spacing.1].concat(),
                )
            } else {
                spacing
            }
        } else {
            (Vec::new(), Vec::new())
        };

        // Report whichever allowed style the table is closest to.
        let mut chosen = errors_if_aligned;
        if compact_allowed && (errors_if_compact.len() < chosen.len() || !aligned_allowed) {
            chosen = std::mem::take(&mut errors_if_compact);
        }
        if tight_allowed
            && (errors_if_tight.len() < chosen.len() || (!aligned_allowed && !compact_allowed))
        {
            chosen = std::mem::take(&mut errors_if_tight);
        }

        for error in chosen {
            self.violations.push(RuleViolation::new(
                &MD060,
                format!("{} [{}]", MD060.description, error.message),
                self.context.file_path.clone(),
                range_from_node_range(&error.node.range()),
            ));
        }
    }
}

impl RuleLinter for MD060Linter {
    fn feed(&mut self, node: &Node) {
        if node.kind() == "pipe_table" {
            self.check_table(node);
        }
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        std::mem::take(&mut self.violations)
    }
}

pub const MD060: Rule = Rule {
    id: "MD060",
    alias: "table-column-style",
    tags: &["table"],
    description: "Table column style",
    rule_type: RuleType::Token,
    required_nodes: &["pipe_table"],
    new_linter: |context| Box::new(MD060Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{RuleSeverity, TableColumnStyle};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_rules;

    const ALIGNED: &str = "| Character | Meaning |
| --------- | ------- |
| Y         | Yes     |
| N         | No      |";

    const COMPACT: &str = "| Character | Meaning |
| --- | --- |
| Y | Yes |
| N | No |";

    const TIGHT: &str = "|Character|Meaning|
|---|---|
|Y|Yes|
|N|No|";

    /// Header aligned, body rows a mixture of both.
    const MIXED: &str = "| Character | Meaning |
| --- | --- |
| Y         | Yes |
| N | No      |";

    fn test_config() -> crate::config::QuickmarkConfig {
        test_config_with_rules(vec![("table-column-style", RuleSeverity::Error)])
    }

    fn count(input: &str, style: TableColumnStyle, aligned_delimiter: bool) -> usize {
        let mut config = test_config();
        config.linters.settings.table_column_style = crate::config::MD060TableColumnStyleTable {
            style,
            aligned_delimiter,
        };
        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        linter
            .analyze()
            .iter()
            .filter(|v| v.rule().id == "MD060")
            .count()
    }

    // Every count below was checked against markdownlint-cli2 v0.23.3 (markdownlint v0.41.1).
    /// The header and delimiter row the display-width cases below share: both put their last pipe
    /// at display column fifty.
    const WIDE_HEADER: &str = "| aaa   | bbb                                     |\n| ----- | --------------------------------------- |\n";

    /// A body row holding `cell`, padded so its last pipe lands at display column fifty once the
    /// cell's own width is allowed for.
    fn wide_row(cell: &str, padding: usize) -> String {
        format!("| ccc   | ddd{cell}{}|\n", " ".repeat(padding))
    }

    /// A pipe's column is its *display* column, so what a cell's characters are worth decides
    /// whether the row lines up. Every table here is aligned and every count is markdownlint's.
    #[test]
    fn display_width_decides_alignment() {
        // `string-width` skips control characters, combining marks and zero-width ones; a tab is
        // "ignored by design". None of them shift the pipes after them.
        for cell in [
            "\u{5}", "\u{7f}", "\u{85}", "\u{200b}", "\u{ad}", "\u{301}", "\t",
        ] {
            let table = format!("{WIDE_HEADER}{}", wide_row(cell, 37));
            assert_eq!(
                0,
                count(&table, TableColumnStyle::Aligned, false),
                "cell {cell:?}"
            );
        }
        // A CJK ideograph and an emoji are two columns each, so the padding is two shorter.
        for cell in ["\u{4e2d}", "\u{1f600}"] {
            let table = format!("{WIDE_HEADER}{}", wide_row(cell, 35));
            assert_eq!(
                0,
                count(&table, TableColumnStyle::Aligned, false),
                "cell {cell:?}"
            );
        }
    }

    /// One column short of aligned. `unicode-width` counts a control character as one column where
    /// `string-width` counts none, so before the row was measured without it a stray C0 byte left
    /// the row looking level with the header and nothing was reported.
    #[test]
    fn a_row_one_column_short_is_not_aligned() {
        let table = format!("{WIDE_HEADER}{}", wide_row("\u{5}", 36));
        assert_eq!(1, count(&table, TableColumnStyle::Aligned, false));
        let table = format!("{WIDE_HEADER}{}", wide_row("\u{4e2d}", 34));
        assert_eq!(1, count(&table, TableColumnStyle::Aligned, false));
    }

    /// `string-width` strips ANSI escape sequences before measuring and this does not, so a cell
    /// holding one measures five columns too wide for `\u{1b}[31m` and an aligned row looks
    /// misaligned. markdownlint reports nothing here.
    #[test]
    fn an_ansi_escape_in_a_cell_is_a_known_difference() {
        let table = format!("{WIDE_HEADER}{}", wide_row("\u{1b}[31m", 37));
        assert_eq!(1, count(&table, TableColumnStyle::Aligned, false));
    }

    #[test]
    fn test_any_style_accepts_each_pure_style() {
        assert_eq!(0, count(ALIGNED, TableColumnStyle::Any, false));
        assert_eq!(0, count(COMPACT, TableColumnStyle::Any, false));
        assert_eq!(0, count(TIGHT, TableColumnStyle::Any, false));
    }

    #[test]
    fn test_any_style_reports_a_mixed_table() {
        assert_eq!(2, count(MIXED, TableColumnStyle::Any, false));
    }

    #[test]
    fn test_aligned_style_rejects_other_styles() {
        assert_eq!(0, count(ALIGNED, TableColumnStyle::Aligned, false));
        assert_eq!(5, count(COMPACT, TableColumnStyle::Aligned, false));
        assert_eq!(6, count(TIGHT, TableColumnStyle::Aligned, false));
    }

    #[test]
    fn test_compact_style() {
        assert_eq!(0, count(COMPACT, TableColumnStyle::Compact, false));
        assert_eq!(4, count(ALIGNED, TableColumnStyle::Compact, false));
        assert_eq!(16, count(TIGHT, TableColumnStyle::Compact, false));
    }

    #[test]
    fn test_tight_style() {
        assert_eq!(0, count(TIGHT, TableColumnStyle::Tight, false));
        assert_eq!(16, count(ALIGNED, TableColumnStyle::Tight, false));
        assert_eq!(16, count(COMPACT, TableColumnStyle::Tight, false));
    }

    #[test]
    fn test_aligned_delimiter_only_checks_the_delimiter_row() {
        assert_eq!(0, count(ALIGNED, TableColumnStyle::Any, true));
        assert_eq!(1, count(COMPACT, TableColumnStyle::Any, true));
        assert_eq!(2, count(TIGHT, TableColumnStyle::Any, true));
    }
}
