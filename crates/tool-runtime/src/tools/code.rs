//! `code.symbols` — language-aware symbol-definition scan over the
//! workspace, and `code.diagnostics` — navigation from diagnostic output
//! (`file:line:col` positions) to their source context.
//!
//! Both are deliberately local and lightweight: line-level lexical rules,
//! no language server, no index, no embeddings or vector storage
//! (AGENTS.md invariant 8). They are catalog-optional first-party tools:
//! the model loads them when a task needs precise symbol or diagnostic
//! navigation, and they obey the same output bounds, confinement and
//! artifact spill as every other tool.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use agent_contracts::{
    AgentError, AgentResult, CancellationToken, RunId, ToolOutcome, ToolOutput, ToolRisk, ToolSpec,
};
use agent_workspace::Workspace;
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use super::{Tool, display_relative, walk_files};

// ---------------------------------------------------------------------------
// `code.symbols`
// ---------------------------------------------------------------------------

const MAX_FILES_SCANNED: usize = 5_000;
const MAX_BYTES_PER_FILE: u64 = 2 * 1024 * 1024;
const MODEL_SYMBOLS: usize = 100;

/// One lexical symbol rule: a per-line regex plus how to read the result.
/// `Fixed(kind)` means capture group 1 is the symbol name; `FromGroups`
/// means group 1 is the kind (const/let/var/...) and group 2 the name.
#[derive(Clone, Copy)]
enum RuleKind {
    Fixed(&'static str),
    FromGroups,
}

#[derive(Clone, Copy)]
struct SymbolRule {
    regex: &'static str,
    kind: RuleKind,
}

const fn rule(regex: &'static str, kind: RuleKind) -> SymbolRule {
    SymbolRule { regex, kind }
}

const fn fixed(regex: &'static str, kind: &'static str) -> SymbolRule {
    rule(regex, RuleKind::Fixed(kind))
}

struct Lang {
    rules: &'static [(Regex, RuleKind)],
    /// Optional last-identifier-before-`(` fallback for C-like function
    /// declarations. Explicitly a heuristic: documented as such in the
    /// tool description.
    c_like: Option<&'static str>,
}

const fn lang(rules: &'static [(Regex, RuleKind)], c_like: Option<&'static str>) -> Lang {
    Lang { rules, c_like }
}

fn compile(rules: &[SymbolRule]) -> Vec<(Regex, RuleKind)> {
    rules
        .iter()
        .map(|r| (Regex::new(r.regex).expect("static symbol regex"), r.kind))
        .collect()
}

static RUST: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)", "fn"),
        fixed(r"\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)", "struct"),
        fixed(r"\benum\s+([A-Za-z_][A-Za-z0-9_]*)", "enum"),
        fixed(r"\btrait\s+([A-Za-z_][A-Za-z0-9_]*)", "trait"),
        fixed(r"\bimpl\s+([A-Za-z_][A-Za-z0-9_:]*)", "impl"),
        fixed(r"\bconst\s+([A-Za-z_][A-Za-z0-9_]*)\s*:", "const"),
        fixed(
            r"\bstatic\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*:",
            "static",
        ),
        fixed(r"\btype\s+([A-Za-z_][A-Za-z0-9_]*)\s*=", "type"),
        fixed(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)", "mod"),
        fixed(r"macro_rules!\s+([A-Za-z_][A-Za-z0-9_]*)", "macro"),
    ])
});

static PYTHON: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)", "def"),
        fixed(r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
    ])
});

static TYPESCRIPT: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\bfunction\s+([A-Za-z_$][A-Za-z0-9_$]*)", "function"),
        fixed(r"\bclass\s+([A-Za-z_$][A-Za-z0-9_$]*)", "class"),
        fixed(r"\binterface\s+([A-Za-z_$][A-Za-z0-9_$]*)", "interface"),
        fixed(r"\btype\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=", "type"),
        rule(
            r"\b(?:export\s+)?(const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=",
            RuleKind::FromGroups,
        ),
    ])
});

static GO: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(
            r"\bfunc\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)",
            "func",
        ),
        fixed(
            r"\btype\s+([A-Za-z_][A-Za-z0-9_]*)\s+(?:struct|interface)\b",
            "type",
        ),
        fixed(r"\bvar\s+([A-Za-z_][A-Za-z0-9_]*)\s*=", "var"),
        fixed(r"\bconst\s+([A-Za-z_][A-Za-z0-9_]*)\s*=", "const"),
    ])
});

static C_LIKE: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)", "struct"),
        fixed(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
        fixed(r"\benum\s+([A-Za-z_][A-Za-z0-9_]*)", "enum"),
        fixed(r"\bunion\s+([A-Za-z_][A-Za-z0-9_]*)", "union"),
    ])
});

static JAVA: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
        fixed(r"\binterface\s+([A-Za-z_][A-Za-z0-9_]*)", "interface"),
        fixed(r"\benum\s+([A-Za-z_][A-Za-z0-9_]*)", "enum"),
        fixed(r"\brecord\s+([A-Za-z_][A-Za-z0-9_]*)", "record"),
    ])
});

static CSHARP: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
        fixed(r"\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)", "struct"),
        fixed(r"\binterface\s+([A-Za-z_][A-Za-z0-9_]*)", "interface"),
        fixed(r"\benum\s+([A-Za-z_][A-Za-z0-9_]*)", "enum"),
        fixed(r"\brecord\s+([A-Za-z_][A-Za-z0-9_]*)", "record"),
    ])
});

static KOTLIN: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\bfun\s+([A-Za-z_][A-Za-z0-9_]*)", "fun"),
        fixed(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
        fixed(r"\binterface\s+([A-Za-z_][A-Za-z0-9_]*)", "interface"),
        fixed(r"\bobject\s+([A-Za-z_][A-Za-z0-9_]*)", "object"),
        fixed(
            r"\b(?:val|var)\s+([A-Za-z_][A-Za-z0-9_]*)\s*[:=]",
            "property",
        ),
    ])
});

static SCALA: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\bdef\s+([A-Za-z_][A-Za-z0-9_]*)", "def"),
        fixed(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
        fixed(r"\bobject\s+([A-Za-z_][A-Za-z0-9_]*)", "object"),
        fixed(r"\btrait\s+([A-Za-z_][A-Za-z0-9_]*)", "trait"),
    ])
});

static SWIFT: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\bfunc\s+([A-Za-z_][A-Za-z0-9_]*)", "func"),
        fixed(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
        fixed(r"\bstruct\s+([A-Za-z_][A-Za-z0-9_]*)", "struct"),
        fixed(r"\benum\s+([A-Za-z_][A-Za-z0-9_]*)", "enum"),
        fixed(r"\bprotocol\s+([A-Za-z_][A-Za-z0-9_]*)", "protocol"),
        fixed(
            r"\b(?:var|let)\s+([A-Za-z_][A-Za-z0-9_]*)\s*[:=]",
            "property",
        ),
    ])
});

static ZIG: LazyLock<Vec<(Regex, RuleKind)>> = LazyLock::new(|| {
    compile(&[
        fixed(r"\b(?:pub\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)", "fn"),
        fixed(
            r"\b(?:pub\s+)?(?:const|var)\s+([A-Za-z_][A-Za-z0-9_]*)\s*=",
            "binding",
        ),
        fixed(
            r"\b(?:pub\s+)?(?:struct|enum|union)\s+([A-Za-z_][A-Za-z0-9_]*)",
            "type",
        ),
    ])
});

static LANGS: LazyLock<HashMap<&'static str, Lang>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    map.insert("rs", lang(&RUST, None));
    map.insert("py", lang(&PYTHON, None));
    map.insert("js", lang(&TYPESCRIPT, None));
    map.insert("jsx", lang(&TYPESCRIPT, None));
    map.insert("mjs", lang(&TYPESCRIPT, None));
    map.insert("cjs", lang(&TYPESCRIPT, None));
    map.insert("ts", lang(&TYPESCRIPT, None));
    map.insert("tsx", lang(&TYPESCRIPT, None));
    map.insert("go", lang(&GO, None));
    map.insert("c", lang(&C_LIKE, Some("fn")));
    map.insert("h", lang(&C_LIKE, Some("fn")));
    map.insert("cpp", lang(&C_LIKE, Some("fn")));
    map.insert("hpp", lang(&C_LIKE, Some("fn")));
    map.insert("cc", lang(&C_LIKE, Some("fn")));
    map.insert("hh", lang(&C_LIKE, Some("fn")));
    map.insert("cxx", lang(&C_LIKE, Some("fn")));
    map.insert("hxx", lang(&C_LIKE, Some("fn")));
    map.insert("java", lang(&JAVA, Some("method")));
    map.insert("cs", lang(&CSHARP, Some("method")));
    map.insert("kt", lang(&KOTLIN, None));
    map.insert("kts", lang(&KOTLIN, None));
    map.insert("scala", lang(&SCALA, None));
    map.insert("swift", lang(&SWIFT, None));
    map.insert("zig", lang(&ZIG, None));
    map
});

/// Last-identifier-before-`(` heuristic for C-like function declarations.
/// Lines ending in `;` are calls/statements, control-flow keywords are
/// excluded, and the token directly before the first `(` becomes the name.
fn c_like_function(line: &str) -> Option<&str> {
    const CONTROL: &[&str] = &[
        "if", "for", "while", "switch", "return", "else", "do", "catch", "sizeof", "case", "new",
        "delete", "throw", "try", "goto",
    ];
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.ends_with(';') {
        return None;
    }
    for keyword in CONTROL {
        if trimmed.starts_with(keyword)
            && trimmed[keyword.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        {
            return None;
        }
    }
    let before = &trimmed[..trimmed.find('(')?];
    before
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .rfind(|token| !token.is_empty())
}

/// Byte offset of the first inline comment start (`//` or `/*`), so a
/// symbol regex that happens to match inside a trailing comment is
/// ignored. `line.len()` means "no inline comment on this line".
fn inline_comment_at(line: &str) -> usize {
    let slash_slash = line.find("//").unwrap_or(line.len());
    let slash_star = line.find("/*").unwrap_or(line.len());
    slash_slash.min(slash_star)
}

/// Scan one line for its first symbol definition. Returns
/// `(name, kind, column)` with the column 1-based in characters.
fn scan_line(line: &str, lang: &Lang) -> Option<(String, String, usize)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with('#')
    {
        return None;
    }
    let comment_at = inline_comment_at(line);
    for (regex, kind) in lang.rules {
        let Some(captures) = regex.captures(line) else {
            continue;
        };
        let (name, kind, start) = match kind {
            RuleKind::Fixed(kind) => {
                let name_match = captures.get(1)?;
                (name_match.as_str(), *kind, name_match.start())
            }
            RuleKind::FromGroups => {
                let name_match = captures.get(2)?;
                (
                    name_match.as_str(),
                    captures.get(1)?.as_str(),
                    name_match.start(),
                )
            }
        };
        if start < comment_at {
            return Some((
                name.to_string(),
                kind.to_string(),
                line[..start].chars().count() + 1,
            ));
        }
    }
    if let Some(fallback_kind) = lang.c_like
        && let Some(name) = c_like_function(line)
        && let Some(start) = line.rfind(name)
        && start < comment_at
    {
        return Some((
            name.to_string(),
            fallback_kind.to_string(),
            line[..start].chars().count() + 1,
        ));
    }
    None
}

fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

pub struct CodeSymbolsTool {
    workspace: Workspace,
}

impl CodeSymbolsTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct SymbolsArgs {
    #[serde(default)]
    path: String,
    /// Optional case-sensitive substring filter on the symbol name.
    #[serde(default)]
    query: String,
    #[serde(default = "default_symbols_limit")]
    limit: usize,
    /// Opaque paging token from a previous `code.symbols` result.
    #[serde(default)]
    cursor: Option<String>,
}

fn default_symbols_limit() -> usize {
    200
}

#[async_trait]
impl Tool for CodeSymbolsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "code.symbols".into(),
            description: "Language-aware symbol-definition scan over workspace source files (pure local lexical scan; no language server, no index). Returns `file:line:col  kind name` rows for definitions like fn/struct/class/def/func; comments and ignored dirs are skipped; C-like function detection is a heuristic.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Optional workspace-relative directory"},
                    "query": {"type": "string", "description": "Optional case-sensitive substring filter on the symbol name"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 1000},
                    "cursor": {"type": "string", "description": "Opaque token from a previous code.symbols result; serves the next page from that call's snapshot"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let args: SymbolsArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("code.symbols args: {e}")))?;
        if let Some(cursor) = args.cursor.as_deref() {
            return self.page_from_snapshot(run_id, call_id, cursor).await;
        }
        let limit = args.limit.clamp(1, 1_000);
        let root = self.workspace.resolve_relative(&args.path).await?;

        let mut files = Vec::new();
        let mut budget = MAX_FILES_SCANNED;
        walk_files(&root, &mut files, &mut budget, None).await?;
        files.sort();

        let query = args.query;
        let mut symbols: Vec<(String, usize, usize, String, String)> = Vec::new();
        let mut scanned_files = 0usize;

        'files: for file in files {
            let metadata = match tokio::fs::metadata(&file).await {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.len() > MAX_BYTES_PER_FILE {
                continue;
            }
            let Ok(text) = tokio::fs::read_to_string(&file).await else {
                continue;
            };
            let Some(extension) = extension_of(&file) else {
                continue;
            };
            let Some(lang) = LANGS.get(extension.as_str()) else {
                continue;
            };
            scanned_files += 1;
            let relative = display_relative(&self.workspace, &file);
            for (index, line) in text.lines().enumerate() {
                let Some((name, kind, column)) = scan_line(line, lang) else {
                    continue;
                };
                if !query.is_empty() && !name.contains(&query) {
                    continue;
                }
                symbols.push((relative.clone(), index + 1, column, kind, name));
                if symbols.len() >= limit {
                    break 'files;
                }
            }
        }
        symbols.sort();

        let model_face = MODEL_SYMBOLS.min(symbols.len());
        let model_rows = symbols.iter().take(model_face).cloned().collect::<Vec<_>>();
        let full = symbols
            .iter()
            .map(|(path, line, column, kind, name)| {
                format!("{path}:{line}:{column}  {kind} {name}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let artifact_ref = if symbols.len() > model_face {
            Some(
                self.workspace
                    .write_artifact(run_id, "code-symbols", "txt", full.as_bytes())
                    .await?,
            )
        } else {
            None
        };
        let (cursor, has_more) = match &artifact_ref {
            Some(reference) => (Some(format!("{reference}#{model_face}")), true),
            None => (None, false),
        };
        let truncated_note = artifact_ref
            .as_ref()
            .map(|r| {
                format!(
                    "\n... {} more symbols; full list: {r}",
                    symbols.len() - model_rows.len()
                )
            })
            .unwrap_or_default();

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "code.symbols".into(),
            ok: true,
            summary: format!("{} symbols across {} files", symbols.len(), scanned_files),
            model_content: if model_rows.is_empty() {
                "no symbols found".to_string()
            } else {
                format!(
                    "{}{}",
                    model_rows
                        .iter()
                        .map(|(path, line, column, kind, name)| {
                            format!("{path}:{line}:{column}  {kind} {name}")
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    truncated_note
                )
            },
            artifact_ref,
            metadata: json!({
                "symbols": symbols.len(),
                "files_scanned": scanned_files,
                "returned": model_rows.len(),
                "has_more": has_more,
                "cursor": cursor,
            }),
        }))
    }
}

impl CodeSymbolsTool {
    /// Serve one page from a previous call's snapshot artifact (cursor is
    /// `<artifact_ref>#<offset>`), capped at `MODEL_SYMBOLS` rows per page
    /// like the first page.
    async fn page_from_snapshot(
        &self,
        run_id: RunId,
        call_id: &str,
        cursor: &str,
    ) -> AgentResult<ToolOutcome> {
        use super::{parse_cursor, read_snapshot_lines};

        let (reference, offset) = parse_cursor(cursor)?;
        let lines = read_snapshot_lines(&self.workspace, run_id, reference).await?;
        if offset > lines.len() {
            return Err(AgentError::InvalidRequest(format!(
                "cursor is past the end of the snapshot ({offset} > {} lines)",
                lines.len()
            )));
        }
        let page: Vec<&str> = lines
            .iter()
            .skip(offset)
            .take(MODEL_SYMBOLS)
            .map(String::as_str)
            .collect();
        let next_offset = offset + page.len();
        let has_more = next_offset < lines.len();
        let next_cursor = has_more.then(|| format!("{reference}#{next_offset}"));

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "code.symbols".into(),
            ok: true,
            summary: format!(
                "symbol rows {}-{} of {} (snapshot)",
                offset + 1,
                next_offset,
                lines.len()
            ),
            model_content: if page.is_empty() {
                "no more symbols".to_string()
            } else {
                page.join("\n")
            },
            artifact_ref: Some(reference.to_string()),
            metadata: json!({
                "symbols": lines.len(),
                "returned": page.len(),
                "has_more": has_more,
                "cursor": next_cursor,
            }),
        }))
    }
}

// ---------------------------------------------------------------------------
// `code.diagnostics`
// ---------------------------------------------------------------------------

/// Diagnostic position regex: `<file>:<line>[:<col>]`. The file part is
/// constrained to known source/config extensions so prose like URLs or
/// timestamps (`example.com:8080`) cannot be misread as a position.
static POSITION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)\b(?P<f>(?:\./)?[A-Za-z0-9_][A-Za-z0-9_./\\-]*\.(?:rs|py|js|jsx|mjs|cjs|ts|tsx|go|c|h|cpp|hpp|cc|hh|cxx|hxx|java|cs|kt|kts|scala|swift|zig|toml|json|md|txt|sh|bat|ps1|sql|xml|yml|yaml|css|html))(?::(?P<l>[0-9]+))(?::(?P<c>[0-9]+))?",
    )
    .expect("static diagnostic position regex")
});

const MAX_DIAG_MODEL_CHARS: usize = 12_000;
const MAX_DIAG_LINE_CHARS: usize = 200;
const MAX_DIAG_INPUT_CHARS: usize = 100_000;
const MAX_DIAG_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_DIAG_POSITIONS: usize = 50;

pub struct CodeDiagnosticsTool {
    workspace: Workspace,
}

impl CodeDiagnosticsTool {
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace }
    }
}

#[derive(Deserialize)]
struct DiagnosticsArgs {
    /// Raw diagnostic text (compiler/lint output) containing
    /// `file:line:col` positions to expand.
    input: String,
    /// Source context lines to show around each position.
    #[serde(default = "default_context")]
    context: usize,
    /// Max positions to expand, in input order.
    #[serde(default = "default_diag_limit")]
    limit: usize,
}

fn default_context() -> usize {
    3
}

fn default_diag_limit() -> usize {
    20
}

#[async_trait]
impl Tool for CodeDiagnosticsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "code.diagnostics".into(),
            description: "Expand diagnostic output into source context. Pass compiler/lint output; every `file:line:col` position (rustc `-->` style included) is resolved as a workspace-relative path and shown with its surrounding lines, so the model can jump from an error message straight to the code. Unresolvable or escaping paths are reported, never followed; output is bounded.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["input"],
                "properties": {
                    "input": {"type": "string", "description": "Raw diagnostic text containing file:line:col positions"},
                    "context": {"type": "integer", "minimum": 0, "maximum": 10, "description": "Source lines around each position (default 3)"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Max positions to expand (default 20)"}
                }
            }),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
        }
    }

    async fn execute(
        &self,
        _run_id: RunId,
        call_id: &str,
        arguments: Value,
        _effect_context: Option<agent_contracts::OperationEffectContext>,
        _cancel: CancellationToken,
    ) -> AgentResult<ToolOutcome> {
        let mut args: DiagnosticsArgs = serde_json::from_value(arguments)
            .map_err(|e| AgentError::InvalidRequest(format!("code.diagnostics args: {e}")))?;
        let context = args.context.min(10);
        let limit = args.limit.clamp(1, MAX_DIAG_POSITIONS);
        let input_truncated = args.input.chars().count() > MAX_DIAG_INPUT_CHARS;
        if input_truncated {
            args.input = args.input.chars().take(MAX_DIAG_INPUT_CHARS).collect();
        }

        // Collect unique positions in input order, deduplicated by file+line.
        let mut positions: Vec<(String, usize, usize)> = Vec::new();
        let mut seen: HashSet<(String, usize)> = HashSet::new();
        for captures in POSITION_RE.captures_iter(&args.input) {
            let file = captures["f"].to_string();
            let line: usize = captures["l"].parse().unwrap_or(1);
            let column: usize = captures
                .name("c")
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(1);
            if seen.insert((file.clone(), line)) {
                positions.push((file, line, column));
            }
            if positions.len() >= limit {
                break;
            }
        }

        // Expand each position, bounded by a hard model-content budget.
        let mut cache: HashMap<String, Result<Vec<String>, String>> = HashMap::new();
        let mut out_lines: Vec<String> = Vec::new();
        let mut out_chars = 0usize;
        let mut expanded = 0usize;
        let mut unresolved = 0usize;
        let mut budget_exhausted = false;
        for (file, line, column) in &positions {
            if budget_exhausted {
                break;
            }
            let block = match read_block(&self.workspace, file, *line, *column, context, &mut cache)
                .await
            {
                Ok(block) => block,
                Err(reason) => {
                    unresolved += 1;
                    out_lines.push(format!("(unresolved) {file}:{line}:{column}  {reason}"));
                    continue;
                }
            };
            let block_chars = block.chars().count();
            if out_chars + block_chars > MAX_DIAG_MODEL_CHARS && !out_lines.is_empty() {
                budget_exhausted = true;
                continue;
            }
            out_chars += block_chars;
            expanded += 1;
            out_lines.push(block);
        }
        let skipped = positions.len() - expanded - unresolved;
        if budget_exhausted {
            out_lines.push(format!(
                "... {skipped} more positions not expanded (bounded output)"
            ));
        }

        Ok(ToolOutcome::Value(ToolOutput {
            call_id: call_id.into(),
            tool_name: "code.diagnostics".into(),
            ok: true,
            summary: format!(
                "expanded {expanded} of {} positions ({unresolved} unresolved)",
                positions.len()
            ),
            model_content: if out_lines.is_empty() {
                "no file:line:col positions found in the input".to_string()
            } else {
                out_lines.join("\n")
            },
            artifact_ref: None,
            metadata: json!({
                "positions": positions.len(),
                "expanded": expanded,
                "unresolved": unresolved,
                "skipped": skipped,
                "input_truncated": input_truncated,
            }),
        }))
    }
}

/// Read the source context block for one position: the line itself plus
/// `context` lines around it, prefixed with a `== file:line:col ==` header.
/// File contents are cached per path and read through the confined
/// directory-handle descent (CORE-07); escaping or oversized files yield a
/// `String` reason instead of an error.
async fn read_block(
    workspace: &Workspace,
    file: &str,
    line: usize,
    column: usize,
    context: usize,
    cache: &mut HashMap<String, Result<Vec<String>, String>>,
) -> Result<String, String> {
    let lines = match cache.entry(file.to_string()) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => entry.insert(read_file_lines(workspace, file).await).clone(),
    };
    let lines = lines?;
    let total = lines.len();
    let start = line.saturating_sub(context + 1);
    let end = (line + context).min(total);
    let mut block = format!("== {file}:{line}:{column} ==\n");
    for index in start..end {
        let number = index + 1;
        let marker = if number == line { "=>" } else { "  " };
        let content = lines.get(index).map(|s| s.as_str()).unwrap_or_default();
        let clipped: String = content.chars().take(MAX_DIAG_LINE_CHARS).collect();
        block.push_str(&format!("{marker} {number:>5} | {clipped}\n"));
    }
    Ok(block)
}

async fn read_file_lines(workspace: &Workspace, file: &str) -> Result<Vec<String>, String> {
    // `confined_open_read` takes the workspace-relative path directly: it
    // validates it (lexical + link-swap pinned descent) and opens it from
    // the root handle. Resolving first would hand it an absolute path,
    // which `clean_relative` correctly refuses.
    let confined = workspace
        .confined_open_read(file)
        .await
        .map_err(|e| e.to_string())?;
    let metadata = confined.metadata().map_err(|e| e.to_string())?;
    if metadata.len() > MAX_DIAG_FILE_BYTES {
        return Err(format!(
            "file is {} bytes (above the {} byte expand cap)",
            metadata.len(),
            MAX_DIAG_FILE_BYTES
        ));
    }
    let mut text = String::new();
    let mut file = confined.into_tokio();
    file.read_to_string(&mut text)
        .await
        .map_err(|e| e.to_string())?;
    Ok(text.lines().map(String::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{CancellationToken, ToolExecutionRequest};

    fn value(outcome: ToolOutcome) -> ToolOutput {
        match outcome {
            ToolOutcome::Value(output) => output,
            ToolOutcome::PreparedEffect { .. }
            | ToolOutcome::RuntimeDirective { .. }
            | ToolOutcome::EngineQuery { .. } => panic!("code tools must return a plain value"),
        }
    }

    async fn temp_workspace() -> (Workspace, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        (workspace, dir)
    }

    async fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn request(run_id: RunId, name: &str, args: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            run_id,
            call: agent_contracts::ToolCall {
                id: "c".into(),
                name: name.into(),
                arguments: args,
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        }
    }

    async fn execute_symbols(workspace: &Workspace, run_id: RunId, args: Value) -> ToolOutput {
        let tool = CodeSymbolsTool::new(workspace.clone());
        let request = request(run_id, "code.symbols", args);
        let outcome = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        value(outcome)
    }

    #[tokio::test]
    async fn symbols_scan_rust_definitions_with_columns() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(
            &root,
            "src/lib.rs",
            "pub fn auth() {}\n\
             struct User {}\n\
             enum Role {}\n\
             trait Actor {}\n\
             impl User {}\n\
             const LIMIT: u32 = 10;\n\
             static mut COUNT: u32 = 0;\n\
             type Alias = u32;\n\
             mod helpers {}\n\
             macro_rules! make_id {}\n",
        )
        .await;

        let output = execute_symbols(&workspace, RunId::new(), json!({})).await;
        assert!(output.ok, "{}", output.summary);
        let content = output.model_content;
        for expected in [
            "src/lib.rs:1:8  fn auth",
            "src/lib.rs:2:8  struct User",
            "src/lib.rs:3:6  enum Role",
            "src/lib.rs:4:7  trait Actor",
            "src/lib.rs:5:6  impl User",
            "src/lib.rs:6:7  const LIMIT",
            "src/lib.rs:7:12  static COUNT",
            "src/lib.rs:8:6  type Alias",
            "src/lib.rs:9:5  mod helpers",
            "src/lib.rs:10:14  macro make_id",
        ] {
            assert!(content.contains(expected), "missing {expected}: {content}");
        }
    }

    #[tokio::test]
    async fn symbols_scan_multiple_languages() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(
            &root,
            "app.py",
            "def parse(text):\n    return text\nclass Parser:\n    pass\nasync def fetch():\n    pass\n",
        )
        .await;
        write(
            &root,
            "web.ts",
            "export function render() {}\nclass View {}\ninterface Props {}\nconst base = 10;\nexport let counter = 0;\ntype Id = string;\n",
        )
        .await;
        write(
            &root,
            "srv.go",
            "package srv\nfunc main() {}\nfunc (r *Repo) Find(id int) {}\ntype Repo struct {}\nvar global = 1\n",
        )
        .await;

        let output = execute_symbols(&workspace, RunId::new(), json!({})).await;
        assert!(output.ok, "{}", output.summary);
        let content = output.model_content;
        for expected in [
            "app.py:1:5  def parse",
            "app.py:3:7  class Parser",
            "app.py:5:11  def fetch",
            "web.ts:1:17  function render",
            "web.ts:2:7  class View",
            "web.ts:3:11  interface Props",
            "web.ts:4:7  const base",
            "web.ts:5:12  let counter",
            "web.ts:6:6  type Id",
            "srv.go:2:6  func main",
            "srv.go:3:16  func Find",
            "srv.go:4:6  type Repo",
            "srv.go:5:5  var global",
        ] {
            assert!(content.contains(expected), "missing {expected}: {content}");
        }
    }

    #[tokio::test]
    async fn symbols_skip_comments_and_ignored_dirs() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(
            &root,
            "src/lib.rs",
            "// fn fake() {}\nfn real() {}\nlet x = 1; // fn trailing()\n",
        )
        .await;
        write(&root, "target/gen.rs", "fn generated() {}\n").await;
        write(&root, ".focus-agent/traces/x.rs", "fn traced() {}\n").await;

        let output = execute_symbols(&workspace, RunId::new(), json!({})).await;
        let content = output.model_content;
        assert!(
            content.contains("fn real"),
            "real symbol missing: {content}"
        );
        assert!(
            !content.contains("fake"),
            "comment symbol leaked: {content}"
        );
        assert!(
            !content.contains("trailing"),
            "inline comment leaked: {content}"
        );
        assert!(
            !content.contains("generated"),
            "ignored dir leaked: {content}"
        );
        assert!(!content.contains("traced"), "state dir leaked: {content}");
    }

    #[tokio::test]
    async fn symbols_query_filter_and_c_like_heuristic() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(
            &root,
            "main.c",
            "int main(int argc, char **argv) {\n    return 0;\n}\nvoid helper(int x) {}\nstatic void internal(void) {}\n",
        )
        .await;
        write(
            &root,
            "App.java",
            "public class App {\n    public static void main(String[] args) {}\n    private int compute() { return 1; }\n}\n",
        )
        .await;

        let output = execute_symbols(&workspace, RunId::new(), json!({})).await;
        let content = output.model_content;
        for expected in [
            "main.c:1:5  fn main",
            "main.c:4:6  fn helper",
            "main.c:5:13  fn internal",
            "App.java:1:14  class App",
            "App.java:2:24  method main",
            "App.java:3:17  method compute",
        ] {
            assert!(content.contains(expected), "missing {expected}: {content}");
        }

        let filtered = execute_symbols(&workspace, RunId::new(), json!({"query": "main"})).await;
        let lines: Vec<&str> = filtered.model_content.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "query must filter to fn main + method main: {lines:?}"
        );
    }

    #[tokio::test]
    async fn symbols_bounds_and_pages_consistent_snapshot() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for i in 0..250 {
            body.push_str(&format!("fn func_{i}() {{}}\n"));
        }
        write(&root, "big.rs", &body).await;

        let tool = CodeSymbolsTool::new(workspace.clone());
        let run_id = RunId::new();
        let symbols = |args: Value| {
            let tool = &tool;
            let call = agent_contracts::ToolCall {
                id: "c".into(),
                name: "code.symbols".into(),
                arguments: args,
            };
            async move {
                tool.execute(run_id, "c", call.arguments, None, CancellationToken::new())
                    .await
            }
        };

        let first = value(symbols(json!({"limit": 300})).await.unwrap());
        assert_eq!(first.metadata["symbols"], 250);
        assert!(first.metadata["has_more"].as_bool().unwrap());
        assert!(first.artifact_ref.is_some());
        assert_eq!(first.metadata["returned"], MODEL_SYMBOLS);

        // The file changes between pages; paging must not notice.
        std::fs::write(root.join("big.rs"), "fn changed() {}\n").unwrap();

        let mut total = first.metadata["returned"].as_u64().unwrap();
        let mut has_more = first.metadata["has_more"].as_bool().unwrap();
        let mut cursor = first.metadata["cursor"].as_str().unwrap().to_string();
        while has_more {
            let page = value(
                symbols(json!({"limit": 300, "cursor": cursor}))
                    .await
                    .unwrap(),
            );
            assert_eq!(
                page.metadata["symbols"], 250,
                "total comes from the snapshot"
            );
            assert!(
                !page.model_content.contains("changed"),
                "snapshot must not see later edits"
            );
            total += page.metadata["returned"].as_u64().unwrap();
            has_more = page.metadata["has_more"].as_bool().unwrap();
            if has_more {
                cursor = page.metadata["cursor"].as_str().unwrap().to_string();
            }
        }
        assert_eq!(total, 250, "all pages together must cover the snapshot");
    }

    async fn execute_diagnostics(workspace: &Workspace, run_id: RunId, args: Value) -> ToolOutput {
        let tool = CodeDiagnosticsTool::new(workspace.clone());
        let request = request(run_id, "code.diagnostics", args);
        let outcome = tool
            .execute(run_id, "c", request.call.arguments, None, request.cancel)
            .await
            .unwrap();
        value(outcome)
    }

    #[tokio::test]
    async fn diagnostics_expands_rustc_style_positions_with_context() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(
            &root,
            "src/main.rs",
            "fn main() {\n    let x = 1;\n    let y = x + 1;\n    println!(\"{y}\");\n}\n",
        )
        .await;
        let diagnostic = "error[E0308]: mismatched types\n  --> src/main.rs:3:14\n   |\n3  |     let y = x + 1;\n   |              ^ expected `i32`, found `&str`\n";

        let output = execute_diagnostics(
            &workspace,
            RunId::new(),
            json!({"input": diagnostic, "context": 1}),
        )
        .await;
        assert!(output.ok, "{}", output.summary);
        let content = output.model_content;
        assert!(
            content.contains("== src/main.rs:3:14 =="),
            "header missing: {content}"
        );
        assert!(
            content
                .lines()
                .any(|l| l.starts_with("=>") && l.contains("3 |     let y = x + 1;")),
            "target line marker missing: {content}"
        );
        assert!(
            content.lines().any(|l| l.contains("2 |     let x = 1;")),
            "context above: {content}"
        );
        assert!(
            content
                .lines()
                .any(|l| l.contains("4 |     println!(\"{y}\");")),
            "context below: {content}"
        );
        assert_eq!(output.metadata["positions"], 1);
        assert_eq!(output.metadata["expanded"], 1);
        assert_eq!(output.metadata["unresolved"], 0);
    }

    #[tokio::test]
    async fn diagnostics_marks_escaping_and_missing_paths_unresolved() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        write(&root, "src/main.rs", "fn main() {}\n").await;
        // `a/../outside.rs` lexically escapes the workspace: the position
        // regex captures it, then confinement refuses to resolve it. The
        // path is reported, never followed.
        let diagnostic =
            "note: see a/../outside.rs:2:1\nnote: see missing.rs:4:2\nnote: see src/main.rs:1:5\n";

        let output = execute_diagnostics(
            &workspace,
            RunId::new(),
            json!({"input": diagnostic, "context": 0}),
        )
        .await;
        let content = output.model_content;
        assert!(
            content.contains("(unresolved) a/../outside.rs:2:1"),
            "escape must be reported not followed: {content}"
        );
        assert!(
            content.contains("(unresolved) missing.rs:4:2"),
            "missing file must be reported: {content}"
        );
        assert!(
            content.contains("== src/main.rs:1:5 =="),
            "resolvable position must expand: {content}"
        );
        assert_eq!(output.metadata["unresolved"], 2);
        assert_eq!(output.metadata["expanded"], 1);
    }

    #[tokio::test]
    async fn diagnostics_deduplicates_and_bounds_output() {
        let (workspace, _dir) = temp_workspace().await;
        let root = workspace.root().to_path_buf();
        let mut body = String::new();
        for i in 0..60 {
            body.push_str(&format!("fn func_{i}() {{}}\n"));
        }
        write(&root, "big.rs", &body).await;

        // Many distinct positions with wide context must stop at the char
        // budget and report the skipped remainder.
        let mut input = String::new();
        for i in 0..60 {
            input.push_str(&format!("warn: big.rs:{}:1\n", i + 1));
        }
        let output = execute_diagnostics(
            &workspace,
            RunId::new(),
            json!({"input": input, "context": 10, "limit": 50}),
        )
        .await;
        assert_eq!(output.metadata["positions"], 50, "limit caps positions");
        let skipped = output.metadata["skipped"].as_u64().unwrap();
        assert!(skipped > 0, "output must be bounded: {skipped}");
        assert!(
            output.model_content.chars().count() <= MAX_DIAG_MODEL_CHARS + 512,
            "model content must stay bounded"
        );
    }

    #[tokio::test]
    async fn diagnostics_no_positions_is_a_clean_result() {
        let (workspace, _dir) = temp_workspace().await;
        let output = execute_diagnostics(
            &workspace,
            RunId::new(),
            json!({"input": "all clear, nothing to see here"}),
        )
        .await;
        assert!(output.ok);
        assert!(output.model_content.contains("no file:line:col positions"));
        assert_eq!(output.metadata["positions"], 0);
    }
}
