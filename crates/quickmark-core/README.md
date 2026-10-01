# quickmark-core

Lightning-fast Markdown/CommonMark linter core library with comrak based parsing.

## Overview

`quickmark-core` is the foundational library for QuickMark, providing high-performance Markdown linting capabilities. It features an integrated configuration system, comrak based parsing, and a pluggable rule architecture designed for speed and extensibility.

## Features

- **comrak Parsing**: CommonMark plus GFM tables, task lists and front matter, translated into a flat node tree that rules walk
- **Integrated Configuration**: Built-in TOML configuration parsing and validation
- **Rule System**: Pluggable architecture with 5 rule types for optimal performance
- **Single-Pass Architecture**: Efficient processing with cached node filtering
- **Configuration-Driven**: Externally configurable rule severity and settings

## Usage

```rust
use std::path::Path;

use quickmark_core::config::config_in_path_or_default;
use quickmark_core::linter::MultiRuleLinter;

let config = config_in_path_or_default(Path::new("."))?;

// One linter per document: `analyze` consumes it, so build a fresh one for the next file.
let mut linter = MultiRuleLinter::new_for_document(
    Path::new("example.md").to_path_buf(),
    config,
    markdown_content,
);
for violation in linter.analyze() {
    println!("{violation}");
}
```

## Rule Types

- **Line-Based**: Analyze raw text lines (e.g., line length limits)
- **Token-Based**: Work with specific AST node types (e.g., headings, lists)
- **Document-Wide**: Require full document analysis (e.g., duplicate detection)
- **Hybrid**: Need both AST and line context (e.g., spacing rules)
- **Special**: Unique requirements (e.g., external dictionaries)

## License

MIT
