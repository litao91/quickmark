use std::{cell::RefCell, collections::HashMap, fmt::Display, path::PathBuf, rc::Rc};

use crate::{
    ast::{self, FacadeTree, Node, NodeRange},
    config::{QuickmarkConfig, RuleSeverity},
    rules::{Rule, ALL_RULES},
};

#[derive(Debug, Clone)]
pub struct CharPosition {
    pub line: usize,
    pub character: usize,
}

#[derive(Debug, Clone)]
pub struct Range {
    pub start: CharPosition,
    pub end: CharPosition,
}
#[derive(Debug)]
pub struct Location {
    pub file_path: PathBuf,
    pub range: Range,
}

#[derive(Debug)]
pub struct RuleViolation {
    location: Location,
    message: String,
    rule: &'static Rule,
    pub(crate) severity: RuleSeverity,
}

impl RuleViolation {
    pub fn new(rule: &'static Rule, message: String, file_path: PathBuf, range: Range) -> Self {
        Self {
            rule,
            message,
            location: Location { file_path, range },
            severity: RuleSeverity::Error, // Default, will be overridden by MultiRuleLinter
        }
    }

    pub fn location(&self) -> &Location {
        &self.location
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn rule(&self) -> &'static Rule {
        self.rule
    }

    pub fn severity(&self) -> &RuleSeverity {
        &self.severity
    }
}

/// Convert from a node range to library range
pub fn range_from_node_range(node_range: &NodeRange) -> Range {
    Range {
        start: CharPosition {
            line: node_range.start_point.row,
            character: node_range.start_point.column,
        },
        end: CharPosition {
            line: node_range.end_point.row,
            character: node_range.end_point.column,
        },
    }
}

impl Display for RuleViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{} {}/{} {}",
            self.location().file_path.to_string_lossy(),
            self.location().range.start.line,
            self.location().range.start.character,
            self.rule().id,
            self.rule().alias,
            self.message()
        )
    }
}

/// **SINGLE-USE CONTRACT**: Context instances are designed for one-time use only.
///
/// Each Context instance should be used to analyze exactly one source document.
/// The lazy initialization of caches (lines, node_cache) happens once and the
/// context becomes immutable after that point.
///
#[derive(Debug)]
pub struct Context {
    pub file_path: PathBuf,
    pub config: QuickmarkConfig,
    /// Raw text lines for line-based rules (MD013, MD010, etc.) - initialized once per document
    pub lines: RefCell<Vec<String>>,
    /// Cached AST nodes filtered by type for efficient access - initialized once per document
    pub node_cache: RefCell<HashMap<&'static str, Vec<NodeInfo>>>,
    /// Original document content for byte-based access - initialized once per document
    pub document_content: RefCell<String>,
}

/// Lightweight node information for caching
#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub line_start: usize,
    pub line_end: usize,
    pub kind: &'static str,
}

impl Context {
    pub fn new(
        file_path: PathBuf,
        config: QuickmarkConfig,
        source: &str,
        tree: &FacadeTree,
    ) -> Self {
        // Split lines the way the parser does. CommonMark counts a bare `\r` as a line ending, and
        // so do comrak and markdownlint's micromark — but `str::lines` does not, so using it here
        // would number `Context.lines` differently from the tree's rows and make every line-based
        // rule read the wrong line on such a document. A trailing line ending yields a final empty
        // line, which is the extra line markdownlint counts.
        let index = ast::synth::LineIndex::new(source);
        let lines: Vec<String> = if source.is_empty() {
            Vec::new()
        } else {
            (0..index.line_count())
                .map(|row| index.content(row).to_string())
                .collect()
        };
        let node_cache = Self::build_node_cache(tree);

        Self {
            file_path,
            config,
            lines: RefCell::new(lines),
            node_cache: RefCell::new(node_cache),
            document_content: RefCell::new(source.to_string()),
        }
    }

    /// Get the full document content as a string reference
    /// Returns a reference to the original document content stored during initialization
    pub fn get_document_content(&self) -> std::cell::Ref<'_, String> {
        self.document_content.borrow()
    }

    /// Build cache of nodes filtered by type for efficient rule access.
    ///
    /// The tree is stored in pre-order, so walking it by index visits every node in document order
    /// and each kind's bucket comes out line-sorted — the property `md046` and `md048` relied on
    /// when they re-sorted defensively.
    ///
    /// Inline nodes are left out, so `get_nodes` hands a rule one entry per *block* of the kind it
    /// asked for and never an inline descendant it did not.
    fn build_node_cache(tree: &FacadeTree) -> HashMap<&'static str, Vec<NodeInfo>> {
        let mut cache: HashMap<&'static str, Vec<NodeInfo>> = HashMap::new();
        for index in 0..tree.node_count() {
            let node = tree.node(index as u32);
            if node.is_inline() {
                continue;
            }
            cache.entry(node.kind()).or_default().push(NodeInfo {
                line_start: node.start_position().row,
                line_end: node.end_position().row,
                kind: node.kind(),
            });
        }
        cache
    }

    /// Get cached nodes of specific types - optimized equivalent of filterByTypesCached
    pub fn get_nodes(&self, node_types: &[&str]) -> Vec<NodeInfo> {
        let cache = self.node_cache.borrow();
        let mut result = Vec::new();
        for node_type in node_types {
            if let Some(nodes) = cache.get(*node_type) {
                result.extend(nodes.iter().cloned());
            }
        }
        result
    }
}

/// **SINGLE-USE CONTRACT**: RuleLinter instances are designed for one-time use only.
///
/// Each RuleLinter instance should be used to analyze exactly one source document
/// and then discarded. This eliminates the complexity of state management and cleanup:
///
/// - No reset/cleanup methods needed
/// - No state contamination between different documents
/// - Simpler, more predictable behavior
///
/// After calling `analyze()` on a `MultiRuleLinter`, the entire linter and all its
/// rule instances become invalid and should not be reused.
///
/// ## Usage Pattern
/// ```rust,no_run
/// # use quickmark_core::linter::MultiRuleLinter;
/// # use quickmark_core::config::QuickmarkConfig;
/// # use std::path::PathBuf;
/// # let path = PathBuf::new();
/// # let config: QuickmarkConfig = unimplemented!();
/// # let source1 = "";
/// # let source2 = "";
///
/// // Correct: Create fresh linter for each document
/// let mut linter1 = MultiRuleLinter::new_for_document(path.clone(), config.clone(), source1);
/// let violations1 = linter1.analyze(); // Use once, then discard
///
/// // Create new linter for next document
/// let mut linter2 = MultiRuleLinter::new_for_document(path, config, source2);
/// let violations2 = linter2.analyze(); // Fresh linter, no contamination
/// ```
pub trait RuleLinter {
    /// Process a single AST node and accumulate state for violation detection.
    ///
    /// **CONTRACT**: This method will be called exactly once per AST node
    /// for a single document analysis session. Rule linters have access to the
    /// document content and parsed data through their initialized Context.
    fn feed(&mut self, node: &Node);

    /// Called after all nodes have been processed to return all violations found.
    ///
    /// **CONTRACT**: This method will be called exactly once at the end of document analysis.
    fn finalize(&mut self) -> Vec<RuleViolation>;
}
/// **SINGLE-USE CONTRACT**: MultiRuleLinter instances are designed for one-time use only.
///
/// Create a fresh MultiRuleLinter for each document you want to analyze using `new_for_document()`.
/// After calling `analyze()`, the linter and all its rule instances should be discarded.
pub struct MultiRuleLinter {
    linters: Vec<Box<dyn RuleLinter>>,
    doc: Option<FacadeTree>,
    config: QuickmarkConfig,
}

impl MultiRuleLinter {
    /// **SINGLE-USE API ENFORCEMENT**: Create a MultiRuleLinter bound to a specific document.
    ///
    /// This constructor enforces the single-use contract by:
    /// 1. Taking the document content immediately
    /// 2. Parsing and initializing the context cache upfront
    /// 3. Creating rule linters with pre-initialized context
    /// 4. Making the linter ready for immediate use with `analyze()`
    ///
    /// After calling `analyze()`, this linter instance should be discarded.
    pub fn new_for_document(file_path: PathBuf, config: QuickmarkConfig, document: &str) -> Self {
        // Early exit optimization: Check if any rules are enabled before expensive operations
        let active_rules: Vec<_> = ALL_RULES
            .iter()
            .filter(|r| {
                config
                    .linters
                    .severity
                    .get(r.alias)
                    .map(|severity| *severity != RuleSeverity::Off)
                    .unwrap_or(false)
            })
            .collect();

        // If no rules are active, create minimal linter that does no work
        if active_rules.is_empty() {
            return Self {
                linters: Vec::new(),
                doc: None,
                config,
            };
        }

        // Parse the document only when we have active rules
        let doc = ast::build::parse(document);

        // Create context with pre-initialized cache only for active rules
        let context = Rc::new(Context::new(file_path, config.clone(), document, &doc));

        // Create rule linters for active rules only
        let linters = active_rules
            .iter()
            .map(|r| (r.new_linter)(context.clone()))
            .collect();

        Self {
            linters,
            doc: Some(doc),
            config,
        }
    }

    /// Analyze the document that was provided during construction.
    ///
    /// **SINGLE-USE CONTRACT**: This method should be called exactly once.
    /// After calling this method, the linter instance should be discarded.
    pub fn analyze(&mut self) -> Vec<RuleViolation> {
        // Early exit optimization: If no linters are active, return immediately
        if self.linters.is_empty() {
            return Vec::new();
        }

        // If we have linters but no document (shouldn't happen), return empty
        let Some(doc) = &self.doc else {
            return Vec::new();
        };

        // Feed all nodes to all linters. Nodes are stored in pre-order, so this visits them in
        // document order with no cursor and no recursion. Inline nodes are skipped: nine rules have
        // both a `match` arm on an inline kind and a regex path over the enclosing `inline` text, so
        // feeding them would report the same violation twice. A rule that wants inline structure
        // walks for it — see `ast::Kind::is_inline`.
        for index in 0..doc.node_count() {
            let node = doc.node(index as u32);
            if node.is_inline() {
                continue;
            }
            for linter in &mut self.linters {
                linter.feed(&node);
            }
        }

        // Collect all violations from finalize and inject severity from config
        let mut violations = Vec::new();
        for linter in &mut self.linters {
            let mut linter_violations = linter.finalize();
            // Inject severity into each violation based on current config
            for violation in &mut linter_violations {
                let severity = self
                    .config
                    .linters
                    .severity
                    .get(violation.rule().alias)
                    .cloned()
                    .unwrap_or(RuleSeverity::Error);
                violation.severity = severity;
            }
            violations.extend(linter_violations);
        }

        violations
    }
}

#[cfg(test)]
mod test {
    use std::{collections::HashMap, path::PathBuf};

    use crate::{
        config::{self, QuickmarkConfig, RuleSeverity},
        rules::{md001::MD001, md003::MD003, md013::MD013},
    };

    use super::{Context, MultiRuleLinter};

    #[test]
    fn test_multiple_violations() {
        let severity: HashMap<_, _> = vec![
            (MD001.alias.to_string(), RuleSeverity::Error),
            (MD003.alias.to_string(), RuleSeverity::Error),
            (MD013.alias.to_string(), RuleSeverity::Error),
        ]
        .into_iter()
        .collect();

        let config = QuickmarkConfig {
            linters: config::LintersTable {
                severity,
                settings: config::LintersSettingsTable {
                    heading_style: config::MD003HeadingStyleTable {
                        style: config::HeadingStyle::ATX,
                    },
                    ..Default::default()
                },
            },
        };

        // This creates a setext h1 after an ATX h1, which should violate:
        // MD003: mixes ATX and setext styles when ATX is enforced
        // It's also at the wrong level for MD001 testing, so let's use a different approach
        let input = "
# First heading
Second heading
==============
#### Fourth level
";

        let mut linter = MultiRuleLinter::new_for_document(PathBuf::from("test.md"), config, input);
        let violations = linter.analyze();
        assert_eq!(
            2,
            violations.len(),
            "Should find both MD001 and MD003 violations"
        );
        assert_eq!(MD001.id, violations[0].rule().id);
        assert_eq!(4, violations[0].location().range.start.line);
        assert_eq!(MD003.id, violations[1].rule().id);
        assert_eq!(2, violations[1].location().range.start.line);
    }

    /// The node cache must hold block kinds only, so that `get_nodes` returns one entry per block of
    /// the kind a rule asked for and never an inline descendant it did not ask for.
    ///
    /// The matching invariant for `feed` — that no rule is handed an inline node — is asserted by the
    /// exact violation counts in md037, md039, md042, md044, md049, md050, md051, md052 and md053,
    /// each of which has a dead `match` arm that would double-report if it were fed.
    #[test]
    fn node_cache_holds_no_inline_kinds() {
        const INLINE: &[&str] = &[
            "text",
            "code_span",
            "emphasis",
            "strong_emphasis",
            "link",
            "image",
            "html_inline",
        ];
        let source = "text *em* **strong** `code` [link](/u) ![img](/i) <b>html</b>\n";
        let tree = crate::ast::build::parse(source);
        let emitted = (0..tree.node_count())
            .map(|index| tree.node(index as u32))
            .filter(|node| node.is_inline())
            .count();
        assert!(
            emitted > 0,
            "the document should have produced inline nodes"
        );

        let context = Context::new(
            PathBuf::from("test.md"),
            QuickmarkConfig::default(),
            source,
            &tree,
        );
        let cache = context.node_cache.borrow();
        let cached: Vec<&str> = cache
            .keys()
            .copied()
            .filter(|kind| INLINE.contains(kind))
            .collect();
        assert!(cached.is_empty(), "cached inline kinds: {cached:?}");
        assert!(
            cache.contains_key("inline"),
            "the `inline` node itself is a block-level child of the paragraph and must stay cached"
        );
    }
}
