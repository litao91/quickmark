use crate::ast::Node;
use serde::Deserialize;
use std::collections::HashSet;
use std::rc::Rc;

use crate::{
    linter::{CharPosition, Context, Range, RuleLinter, RuleViolation},
    rules::{ellipsify, Rule, RuleType},
};

// MD040-specific configuration types
#[derive(Debug, PartialEq, Clone, Deserialize, Default)]
pub struct MD040FencedCodeLanguageTable {
    #[serde(default)]
    pub allowed_languages: Vec<String>,
    #[serde(default)]
    pub language_only: bool,
}

pub(crate) struct MD040Linter {
    context: Rc<Context>,
    violations: Vec<RuleViolation>,
}

impl MD040Linter {
    pub fn new(context: Rc<Context>) -> Self {
        Self {
            context,
            violations: Vec::new(),
        }
    }

    /// The language a fenced code block's opening line specifies, and whether anything follows it.
    ///
    /// The language is the info string's first whitespace-delimited token, verbatim: markdownlint
    /// takes micromark's `codeFencedFenceInfo`, which stops at the first space and keeps everything
    /// else, so `` ```py{#id} `` specifies `py{#id}` and `` ```{.python .numberLines} `` specifies
    /// `{.python`. Cutting either at a `{` reads a language that is not there.
    fn extract_code_block_language<'a>(&self, line: &'a str) -> (Option<&'a str>, bool) {
        // A fenced block inside a block quote or a list item opens on a line that starts with that
        // container's prefix, so `- ```sql` and `> ```java` need it skipped before the fence. The
        // indent limit is unlimited because the tree has already settled that this line opens a
        // fence: inside a list item the fence sits at the item's content column, which is further
        // right than the three spaces CommonMark allows at document level.
        let content = &line[crate::ast::synth::content_column(line.as_bytes(), usize::MAX)..];
        let bytes = content.as_bytes();
        let Some(marker) = bytes.first().copied().filter(|&b| b == b'`' || b == b'~') else {
            return (None, false);
        };

        // The fence is the whole run, not the first three characters: ```` opens a four-backtick
        // fence whose info string starts after the fourth, and taking only three left a stray
        // backtick behind and read it as the language.
        let run = bytes.iter().take_while(|&&b| b == marker).count();
        let info_string = content[run..].trim();

        let mut parts = info_string.split_whitespace();
        match parts.next() {
            // An empty info string has no first token, which is the missing language this reports.
            None => (None, false),
            Some(language) => (Some(language), parts.next().is_some()),
        }
    }
}

impl RuleLinter for MD040Linter {
    fn feed(&mut self, _node: &Node) {
        // MD040 uses Document pattern, not Token pattern
        // All processing happens in finalize()
    }

    fn finalize(&mut self) -> Vec<RuleViolation> {
        let config = &self.context.config.linters.settings.fenced_code_language;
        let node_cache = self.context.node_cache.borrow();
        let lines = self.context.lines.borrow();

        // For performance, convert allowed_languages to a HashSet if it's not empty.
        let allowed_languages_set: Option<HashSet<&str>> = if !config.allowed_languages.is_empty() {
            Some(
                config
                    .allowed_languages
                    .iter()
                    .map(String::as_str)
                    .collect(),
            )
        } else {
            None
        };

        if let Some(fenced_code_blocks) = node_cache.get("fenced_code_block") {
            for node_info in fenced_code_blocks {
                if let Some(first_line) = lines.get(node_info.line_start) {
                    let (language_opt, has_extra_info) =
                        self.extract_code_block_language(first_line);

                    let range = Range {
                        start: CharPosition {
                            line: node_info.line_start,
                            character: 0,
                        },
                        end: CharPosition {
                            line: node_info.line_start,
                            character: first_line.len(),
                        },
                    };

                    let language = match language_opt {
                        Some(lang) => lang,
                        None => {
                            self.violations.push(RuleViolation::new(
                                &MD040,
                                format!(
                                    "{} [Context: \"{}\"]",
                                    MD040.description,
                                    ellipsify(first_line.trim(), false, false)
                                ),
                                self.context.file_path.clone(),
                                range,
                            ));
                            continue;
                        }
                    };

                    if let Some(set) = &allowed_languages_set {
                        if !set.contains(language) {
                            self.violations.push(RuleViolation::new(
                                &MD040,
                                format!("{} [\"{language}\" is not allowed]", MD040.description),
                                self.context.file_path.clone(),
                                range,
                            ));
                            continue;
                        }
                    }

                    // Check if language_only is true and there's extra metadata
                    if config.language_only && has_extra_info {
                        let range = Range {
                            start: CharPosition {
                                line: node_info.line_start,
                                character: 0,
                            },
                            end: CharPosition {
                                line: node_info.line_start,
                                character: first_line.len(),
                            },
                        };
                        let violation = RuleViolation::new(
                            &MD040,
                            format!(
                                "{} [Info string contains more than language: \"{}\"]",
                                MD040.description,
                                first_line.trim()
                            ),
                            self.context.file_path.clone(),
                            range,
                        );
                        self.violations.push(violation);
                    }
                }
            }
        }

        std::mem::take(&mut self.violations)
    }
}

pub const MD040: Rule = Rule {
    id: "MD040",
    alias: "fenced-code-language",
    tags: &["code", "language"],
    description: "Fenced code blocks should have a language specified",
    rule_type: RuleType::Document,
    required_nodes: &["fenced_code_block"],
    new_linter: |context| Box::new(MD040Linter::new(context)),
};

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::config::{LintersSettingsTable, MD040FencedCodeLanguageTable, RuleSeverity};
    use crate::linter::MultiRuleLinter;
    use crate::test_utils::test_helpers::test_config_with_settings;

    fn test_config_default() -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("fenced-code-language", RuleSeverity::Error)],
            LintersSettingsTable {
                fenced_code_language: MD040FencedCodeLanguageTable {
                    allowed_languages: vec![],
                    language_only: false,
                },
                ..Default::default()
            },
        )
    }

    fn test_config_with_allowed_languages(
        allowed_languages: Vec<&str>,
    ) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("fenced-code-language", RuleSeverity::Error)],
            LintersSettingsTable {
                fenced_code_language: MD040FencedCodeLanguageTable {
                    allowed_languages: allowed_languages.iter().map(|s| s.to_string()).collect(),
                    language_only: false,
                },
                ..Default::default()
            },
        )
    }

    fn test_config_with_language_only(language_only: bool) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("fenced-code-language", RuleSeverity::Error)],
            LintersSettingsTable {
                fenced_code_language: MD040FencedCodeLanguageTable {
                    allowed_languages: vec![],
                    language_only,
                },
                ..Default::default()
            },
        )
    }

    fn test_config_with_both_options(
        allowed_languages: Vec<&str>,
        language_only: bool,
    ) -> crate::config::QuickmarkConfig {
        test_config_with_settings(
            vec![("fenced-code-language", RuleSeverity::Error)],
            LintersSettingsTable {
                fenced_code_language: MD040FencedCodeLanguageTable {
                    allowed_languages: allowed_languages.iter().map(|s| s.to_string()).collect(),
                    language_only,
                },
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_fenced_code_with_language_no_violations() {
        let config = test_config_default();
        let input = "# Test

```rust
fn main() {
    println!(\"Hello, World!\");
}
```

```javascript
console.log('Hello, World!');
```

```text
Plain text content
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();
        assert_eq!(md040_violations.len(), 0);
    }

    #[test]
    fn test_fenced_code_without_language_violations() {
        let config = test_config_default();
        let input = "# Test

```
def hello():
    print(\"Hello, World!\")
```

```rust
fn main() {
    println!(\"Hello, World!\");
}
```

```
console.log('Hello, World!');
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find 2 violations: the two fenced code blocks without languages
        assert_eq!(md040_violations.len(), 2);
    }

    #[test]
    fn test_allowed_languages_specific_list() {
        let config = test_config_with_allowed_languages(vec!["rust", "python"]);
        let input = "# Test

```rust
fn main() {}
```

```python
def hello(): pass
```

```javascript
console.log('not allowed');
```

```
no language specified
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find 2 violations: javascript (not in allowed list) and no language
        assert_eq!(md040_violations.len(), 2);
        assert!(md040_violations
            .iter()
            .any(|v| v.message().contains("javascript")));
    }

    #[test]
    fn test_language_only_option_no_extra_info() {
        let config = test_config_with_language_only(true);
        let input = "# Test

```rust
fn main() {}
```

```python {.line-numbers}
def hello(): pass
```

```javascript copy
console.log('Hello');
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find 2 violations: python and javascript have extra info beyond language
        assert_eq!(md040_violations.len(), 2);
    }

    #[test]
    fn test_language_only_option_language_only_allowed() {
        let config = test_config_with_language_only(true);
        let input = "# Test

```rust
fn main() {}
```

```python
def hello(): pass
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find no violations: both have only language specified
        assert_eq!(md040_violations.len(), 0);
    }

    #[test]
    fn test_combined_options() {
        let config = test_config_with_both_options(vec!["rust", "python"], true);
        let input = "# Test

```rust
fn main() {}
```

```python copy
def hello(): pass
```

```javascript
console.log('Hello');
```

```
no language
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find 3 violations:
        // 1. python has extra info (violates language_only)
        // 2. javascript not in allowed list
        // 3. no language specified
        assert_eq!(md040_violations.len(), 3);
    }

    #[test]
    fn test_indented_code_blocks_ignored() {
        let config = test_config_default();
        let input = "# Test

    def hello():
        print(\"This is indented code\")

```
def hello():
    print(\"This is fenced code without language\")
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find only 1 violation: the fenced code block without language
        // Indented code blocks should be ignored
        assert_eq!(md040_violations.len(), 1);
    }

    #[test]
    fn test_case_sensitivity_in_languages() {
        let config = test_config_with_allowed_languages(vec!["rust", "PYTHON"]);
        let input = "# Test

```Rust
fn main() {}
```

```python
def hello(): pass
```

```PYTHON
def hello(): pass
```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find 2 violations: "Rust" and "python" don't match case-sensitive allowed list
        assert_eq!(md040_violations.len(), 2);
    }

    #[test]
    fn test_empty_fenced_code_blocks() {
        let config = test_config_default();
        let input = "# Test

```

```

```rust

```";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find 1 violation: the first block has no language
        assert_eq!(md040_violations.len(), 1);
    }

    #[test]
    fn test_tildes_fenced_code_blocks() {
        let config = test_config_default();
        let input = "# Test

~~~
def hello():
    print(\"Hello\")
~~~

~~~python
def hello():
    print(\"Hello\")
~~~";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        let md040_violations: Vec<_> = violations
            .iter()
            .filter(|v| v.rule().id == "MD040")
            .collect();

        // Should find 1 violation: the first block has no language
        assert_eq!(md040_violations.len(), 1);
    }
    /// A fence inside a container opens on a line that starts with the container's prefix, and the
    /// fence itself is the whole run of backticks or tildes rather than the first three. Getting
    /// either wrong reads the language as missing. Every expectation is a markdownlint-cli2 0.23.3
    /// measurement.
    #[test]
    fn test_reads_the_info_string_past_container_prefixes_and_long_fences() {
        fn rows(input: &str) -> Vec<usize> {
            let mut linter = MultiRuleLinter::new_for_document(
                PathBuf::from("test.md"),
                test_config_default(),
                input,
            );
            linter
                .analyze()
                .iter()
                .filter(|v| v.rule().id == "MD040")
                .map(|v| v.location().range.start.line)
                .collect()
        }

        // The language is there, behind a list marker, a quote marker, both, and an ordered marker.
        assert!(rows("- ```sql\n  x\n").is_empty());
        assert!(rows("> ```java\n> x\n").is_empty());
        assert!(rows("> - ```py\n>   x\n").is_empty());
        assert!(rows("1. ```js\n   x\n").is_empty());
        assert!(rows("  > ```py\n  > x\n").is_empty());

        // Inside a list item the fence sits at the item's content column, further right than the
        // three spaces allowed at document level.
        assert!(rows("1. step\n\n   sub\n\n      ```java\n      x\n").is_empty());

        // The fence is the whole run, so a stray backtick is not the language.
        assert!(rows("- a\n- ````````txt\n  x\n").is_empty());
        assert!(rows("`````lang\nx\n`````\n").is_empty());
        assert!(rows("```py{#id}\nx\n```\n").is_empty());

        // An attribute-style info string specifies a language as far as this rule is concerned:
        // markdownlint takes micromark's `codeFencedFenceInfo`, which is the first token and stops
        // there, so `{.python` is one and a lone `{` is one too.
        assert!(rows("```{.python .numberLines}\nx\n```\n").is_empty());
        assert!(rows("```{.python}\nx\n```\n").is_empty());
        assert!(rows("```{hl_lines=[18]}\nx\n```\n").is_empty());
        assert!(rows("```{.cpp .numberLines startFrom=\"14\"}\nx\n```\n").is_empty());
        assert!(rows("```{.graph .center caption=\"x y\"}\nx\n```\n").is_empty());
        assert!(rows("```{ }\nx\n```\n").is_empty());
        assert!(rows("```{\nx\n```\n").is_empty());
        assert!(rows("```{}\nx\n```\n").is_empty());
        assert!(rows("``` python\nx\n```\n").is_empty());

        // And the ones that genuinely have no language, including a long fence with no info string.
        assert_eq!(vec![0], rows("````\nx\n````\n"));
        assert_eq!(vec![0], rows("~~~\nx\n~~~\n"));
        assert_eq!(vec![0], rows("> ```\n> x\n"));
        assert_eq!(vec![0], rows("- ```\n  x\n"));
        assert_eq!(vec![0], rows("   ```\n   x\n"));
    }

    /// The language is the info string's whole first token, so an allow-list has to name it whole
    /// and `language_only` reads everything after it as meta. Both are markdownlint-cli2 0.23.3
    /// measurements.
    #[test]
    fn test_the_info_token_is_the_language_whole() {
        fn messages(config: crate::config::QuickmarkConfig, input: &str) -> Vec<String> {
            let mut linter =
                MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
            linter
                .analyze()
                .iter()
                .filter(|v| v.rule().id == "MD040")
                .map(|v| v.message().to_string())
                .collect()
        }

        let attributes = "```py{#id}\nx\n```\n";
        assert_eq!(
            vec![
                r#"Fenced code blocks should have a language specified ["py{#id}" is not allowed]"#,
            ],
            messages(test_config_with_allowed_languages(vec!["py"]), attributes)
        );
        assert!(messages(
            test_config_with_allowed_languages(vec!["py{#id}"]),
            attributes
        )
        .is_empty());
        assert!(messages(
            test_config_with_allowed_languages(vec!["{.python}"]),
            "```{.python}\nx\n```\n"
        )
        .is_empty());

        let pandoc = "```{.python .numberLines}\nx\n```\n";
        assert_eq!(
            vec![
                r#"Fenced code blocks should have a language specified [Info string contains more than language: "```{.python .numberLines}"]"#,
            ],
            messages(test_config_with_language_only(true), pandoc)
        );
        assert!(messages(test_config_with_language_only(true), attributes).is_empty());
    }
}
