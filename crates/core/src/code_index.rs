//! Local lexical/symbol code index with a direct-scan fallback.
//!
//! The backend deliberately has no remote embedding dependency, which keeps it usable with
//! domestic and private models.  It persists a compact index keyed by `HEAD + porcelain status`;
//! a corrupt/stale cache is rebuilt, and any build/query failure falls back to an `ignore`-aware
//! line scan so code navigation never depends on index health.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use wyj_api::types::ToolDefinition;

use crate::interfaces::{CodeIndex, CodeIndexStatus, CodeMatch, CodeQuery};
use crate::tool::{Tool, ToolContext, ToolResult};

const INDEX_SCHEMA_VERSION: u32 = 1;
const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_INDEXED_LINES_PER_FILE: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexedLine {
    line: u32,
    symbol: Option<String>,
    kind: Option<String>,
    snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexedFile {
    path: PathBuf,
    language: String,
    len: u64,
    modified_millis: u128,
    lines: Vec<IndexedLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedIndex {
    schema_version: u32,
    root: PathBuf,
    fingerprint: Option<String>,
    revision: Option<String>,
    files: Vec<IndexedFile>,
}

#[derive(Default)]
struct IndexState {
    ready: bool,
    files: BTreeMap<PathBuf, IndexedFile>,
    revision: Option<String>,
    last_error: Option<String>,
    fallback_count: u64,
}

pub struct ProjectCodeIndex {
    root: PathBuf,
    state_path: PathBuf,
    max_file_bytes: u64,
    state: RwLock<IndexState>,
    build_lock: Mutex<()>,
}

impl ProjectCodeIndex {
    pub fn new(root: impl Into<PathBuf>, state_path: impl Into<PathBuf>) -> Result<Self> {
        let root = fs::canonicalize(root.into()).context("canonicalize code-index root")?;
        Ok(Self {
            root,
            state_path: state_path.into(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            state: RwLock::new(IndexState::default()),
            build_lock: Mutex::new(()),
        })
    }

    pub fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes.max(4096);
        self
    }

    pub fn rebuild(&self) -> Result<CodeIndexStatus> {
        let _guard = self.build_lock.lock().unwrap();
        self.rebuild_locked()?;
        Ok(self.status())
    }

    pub fn fallback_count(&self) -> u64 {
        self.state.read().unwrap().fallback_count
    }

    fn ensure_ready(&self) -> Result<()> {
        if self.state.read().unwrap().ready {
            return Ok(());
        }
        let _guard = self.build_lock.lock().unwrap();
        if self.state.read().unwrap().ready {
            return Ok(());
        }
        let fingerprint = workspace_fingerprint(&self.root);
        if let Ok(bytes) = fs::read(&self.state_path) {
            if let Ok(persisted) = serde_json::from_slice::<PersistedIndex>(&bytes) {
                if persisted.schema_version == INDEX_SCHEMA_VERSION
                    && persisted.root == self.root
                    && persisted.fingerprint == fingerprint
                    && persisted_files_are_current(&self.root, &persisted.files)
                {
                    let mut state = self.state.write().unwrap();
                    state.files = persisted
                        .files
                        .into_iter()
                        .map(|file| (file.path.clone(), file))
                        .collect();
                    state.revision = persisted.revision;
                    state.ready = true;
                    state.last_error = None;
                    return Ok(());
                }
            }
        }
        self.rebuild_locked()
    }

    fn rebuild_locked(&self) -> Result<()> {
        match build_files(&self.root, self.max_file_bytes) {
            Ok(files) => {
                let revision = git_revision(&self.root);
                let fingerprint = workspace_fingerprint(&self.root);
                let persisted = PersistedIndex {
                    schema_version: INDEX_SCHEMA_VERSION,
                    root: self.root.clone(),
                    fingerprint,
                    revision: revision.clone(),
                    files: files.values().cloned().collect(),
                };
                if let Some(parent) = self.state_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let tmp = self
                    .state_path
                    .with_extension(format!("json.tmp-{}", std::process::id()));
                fs::write(&tmp, serde_json::to_vec(&persisted)?)?;
                fs::rename(&tmp, &self.state_path)?;
                let mut state = self.state.write().unwrap();
                state.files = files;
                state.revision = revision;
                state.ready = true;
                state.last_error = None;
                Ok(())
            }
            Err(error) => {
                let mut state = self.state.write().unwrap();
                state.ready = false;
                state.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    fn indexed_search(&self, query: &CodeQuery) -> Result<Vec<CodeMatch>> {
        if query.text.trim().is_empty() {
            bail!("code query cannot be empty")
        }
        self.ensure_ready()?;
        let terms = query_terms(&query.text);
        let prefix = normalized_prefix(query.path_prefix.as_deref())?;
        let state = self.state.read().unwrap();
        let mut matches = Vec::new();
        for file in state.files.values() {
            if prefix
                .as_ref()
                .is_some_and(|prefix| !file.path.starts_with(prefix))
            {
                continue;
            }
            if query
                .language
                .as_ref()
                .is_some_and(|language| !file.language.eq_ignore_ascii_case(language))
            {
                continue;
            }
            for line in &file.lines {
                let score = score_line(&terms, &file.path, line);
                if score == 0 {
                    continue;
                }
                matches.push(CodeMatch {
                    path: file.path.clone(),
                    line: line.line,
                    symbol: line.symbol.clone(),
                    kind: line.kind.clone(),
                    snippet: line.snippet.clone(),
                    score_millis: score,
                });
            }
        }
        matches.sort_by(|a, b| {
            b.score_millis
                .cmp(&a.score_millis)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        matches.truncate(query.limit.clamp(1, 200));
        Ok(matches)
    }

    fn fallback_search(&self, query: &CodeQuery) -> Result<Vec<CodeMatch>> {
        let result = direct_scan(&self.root, query, self.max_file_bytes);
        let mut state = self.state.write().unwrap();
        state.fallback_count = state.fallback_count.saturating_add(1);
        result
    }
}

impl CodeIndex for ProjectCodeIndex {
    fn status(&self) -> CodeIndexStatus {
        let state = self.state.read().unwrap();
        CodeIndexStatus {
            backend: if state.last_error.is_some() {
                "lexical+direct_scan(degraded)".to_string()
            } else {
                "lexical+symbol".to_string()
            },
            ready: state.ready,
            indexed_files: state.files.len(),
            revision: state.revision.clone(),
            fallback_available: true,
        }
    }

    fn search(&self, query: &CodeQuery) -> Result<Vec<CodeMatch>> {
        match self.indexed_search(query) {
            Ok(matches) => Ok(matches),
            Err(error) => {
                tracing::warn!("code index failed, using direct scan fallback: {error}");
                self.fallback_search(query)
            }
        }
    }

    fn invalidate(&self, paths: &[PathBuf]) -> Result<()> {
        self.ensure_ready()?;
        let mut state = self.state.write().unwrap();
        for path in paths {
            let path =
                normalized_prefix(Some(path))?.ok_or_else(|| anyhow::anyhow!("empty path"))?;
            let abs = self.root.join(&path);
            if !abs.exists() {
                state.files.remove(&path);
                continue;
            }
            if let Some(file) = index_file(&self.root, &abs, self.max_file_bytes)? {
                state.files.insert(path, file);
            } else {
                state.files.remove(&path);
            }
        }
        state.ready = true;
        state.last_error = None;
        let persisted = PersistedIndex {
            schema_version: INDEX_SCHEMA_VERSION,
            root: self.root.clone(),
            fingerprint: workspace_fingerprint(&self.root),
            revision: state.revision.clone(),
            files: state.files.values().cloned().collect(),
        };
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.state_path, serde_json::to_vec(&persisted)?)?;
        Ok(())
    }
}

pub struct CodeSearchTool {
    index: Arc<dyn CodeIndex>,
}

impl CodeSearchTool {
    pub fn new(index: Arc<dyn CodeIndex>) -> Self {
        Self { index }
    }
}

#[derive(Deserialize)]
struct CodeSearchInput {
    query: String,
    #[serde(default)]
    path_prefix: Option<PathBuf>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for CodeSearchTool {
    fn name(&self) -> &str {
        "CodeSearch"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: "Search the local project code index for symbols and relevant source lines. The index is local-only and automatically falls back to an ignore-aware direct scan if unavailable.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {"type": "string", "minLength": 1},
                    "path_prefix": {"type": "string"},
                    "language": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200}
                },
                "additionalProperties": false
            }),
            native: None,
        }
    }

    async fn run(&self, input: Value, _ctx: &dyn ToolContext) -> Result<ToolResult> {
        let input: CodeSearchInput = serde_json::from_value(input)?;
        let query = CodeQuery {
            text: input.query,
            path_prefix: input.path_prefix,
            language: input.language,
            limit: input.limit.unwrap_or(20),
        };
        let matches = self.index.search(&query)?;
        Ok(ToolResult::ok(serde_json::to_string(&serde_json::json!({
            "status": self.index.status(),
            "matches": matches
        }))?))
    }
}

fn build_files(root: &Path, max_file_bytes: u64) -> Result<BTreeMap<PathBuf, IndexedFile>> {
    let mut files = BTreeMap::new();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
    {
        let entry = entry?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Some(file) = index_file(root, entry.path(), max_file_bytes)? {
            files.insert(file.path.clone(), file);
        }
    }
    Ok(files)
}

fn index_file(root: &Path, path: &Path, max_file_bytes: u64) -> Result<Option<IndexedFile>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > max_file_bytes {
        return Ok(None);
    }
    let Some(language) = language_for(path) else {
        return Ok(None);
    };
    let bytes = fs::read(path)?;
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    let relative = path.strip_prefix(root)?.to_path_buf();
    let mut current_symbol: Option<(String, String)> = None;
    let mut lines = Vec::new();
    for (index, snippet) in text.lines().take(MAX_INDEXED_LINES_PER_FILE).enumerate() {
        if let Some((symbol, kind)) = extract_symbol(snippet, language) {
            current_symbol = Some((symbol, kind));
        }
        if snippet.trim().is_empty() {
            continue;
        }
        lines.push(IndexedLine {
            line: (index + 1) as u32,
            symbol: current_symbol.as_ref().map(|value| value.0.clone()),
            kind: current_symbol.as_ref().map(|value| value.1.clone()),
            snippet: snippet.trim_end().chars().take(500).collect(),
        });
    }
    let modified_millis = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    Ok(Some(IndexedFile {
        path: relative,
        language: language.to_string(),
        len: metadata.len(),
        modified_millis,
        lines,
    }))
}

fn language_for(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "go" => Some("go"),
        "java" => Some("java"),
        "kt" | "kts" => Some("kotlin"),
        "c" | "h" => Some("c"),
        "cc" | "cpp" | "cxx" | "hpp" => Some("cpp"),
        "swift" => Some("swift"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "json" => Some("json"),
        "md" | "mdx" => Some("markdown"),
        "sh" | "bash" | "zsh" => Some("shell"),
        _ => None,
    }
}

fn extract_symbol(line: &str, language: &str) -> Option<(String, String)> {
    let patterns: &[(&str, &str)] = match language {
        "rust" => &[
            (r"\b(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)", "function"),
            (
                r"\b(?:struct|enum|trait|type|mod)\s+([A-Za-z_][A-Za-z0-9_]*)",
                "type",
            ),
            (r"\bimpl(?:<[^>]+>)?\s+([A-Za-z_][A-Za-z0-9_]*)", "impl"),
        ],
        "python" => &[
            (
                r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)",
                "function",
            ),
            (r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
        ],
        "javascript" | "typescript" => &[
            (
                r"\b(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)",
                "function",
            ),
            (
                r"\b(?:class|interface|type|enum)\s+([A-Za-z_$][A-Za-z0-9_$]*)",
                "type",
            ),
            (
                r"\b(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=",
                "binding",
            ),
        ],
        "go" => &[
            (
                r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)",
                "function",
            ),
            (r"^\s*type\s+([A-Za-z_][A-Za-z0-9_]*)", "type"),
        ],
        "java" | "kotlin" | "swift" | "c" | "cpp" => &[
            (
                r"\b(?:class|struct|interface|enum|protocol)\s+([A-Za-z_][A-Za-z0-9_]*)",
                "type",
            ),
            (r"\b(?:fun|func)\s+([A-Za-z_][A-Za-z0-9_]*)", "function"),
        ],
        _ => &[],
    };
    for (pattern, kind) in patterns {
        let regex = Regex::new(pattern).ok()?;
        if let Some(captures) = regex.captures(line) {
            return Some((captures.get(1)?.as_str().to_string(), (*kind).to_string()));
        }
    }
    None
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|term| !term.is_empty())
        .map(|term| term.to_ascii_lowercase())
        .collect()
}

fn score_line(terms: &[String], path: &Path, line: &IndexedLine) -> u32 {
    if terms.is_empty() {
        return 0;
    }
    let path = path.to_string_lossy().to_ascii_lowercase();
    let snippet = line.snippet.to_ascii_lowercase();
    let symbol = line.symbol.as_deref().unwrap_or("").to_ascii_lowercase();
    let mut score = 0_u32;
    for term in terms {
        if symbol == *term {
            score = score.saturating_add(10_000);
        } else if symbol.starts_with(term) {
            score = score.saturating_add(6_000);
        } else if symbol.contains(term) {
            score = score.saturating_add(4_000);
        }
        if path.contains(term) {
            score = score.saturating_add(1_500);
        }
        let occurrences = snippet.matches(term).count().min(10) as u32;
        score = score.saturating_add(occurrences.saturating_mul(800));
    }
    score
}

fn normalized_prefix(path: Option<&Path>) -> Result<Option<PathBuf>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if path.is_absolute() {
        bail!("code-index path_prefix must be project-relative")
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("code-index path_prefix escapes the project root")
            }
        }
    }
    Ok(Some(normalized))
}

fn direct_scan(root: &Path, query: &CodeQuery, max_file_bytes: u64) -> Result<Vec<CodeMatch>> {
    let terms = query_terms(&query.text);
    if terms.is_empty() {
        bail!("code query cannot be empty")
    }
    let prefix = normalized_prefix(query.path_prefix.as_deref())?;
    let mut matches = Vec::new();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
    {
        let entry = entry?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(root)?.to_path_buf();
        if prefix
            .as_ref()
            .is_some_and(|prefix| !relative.starts_with(prefix))
        {
            continue;
        }
        let Some(language) = language_for(entry.path()) else {
            continue;
        };
        if query
            .language
            .as_ref()
            .is_some_and(|expected| !language.eq_ignore_ascii_case(expected))
        {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.len() > max_file_bytes {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let mut current_symbol = None;
        for (index, snippet) in text.lines().enumerate() {
            if let Some((symbol, kind)) = extract_symbol(snippet, language) {
                current_symbol = Some((symbol, kind));
            }
            let line = IndexedLine {
                line: (index + 1) as u32,
                symbol: current_symbol.as_ref().map(|value| value.0.clone()),
                kind: current_symbol.as_ref().map(|value| value.1.clone()),
                snippet: snippet.trim_end().chars().take(500).collect(),
            };
            let score = score_line(&terms, &relative, &line);
            if score > 0 {
                matches.push(CodeMatch {
                    path: relative.clone(),
                    line: line.line,
                    symbol: line.symbol,
                    kind: line.kind,
                    snippet: line.snippet,
                    score_millis: score,
                });
            }
        }
    }
    matches.sort_by(|a, b| {
        b.score_millis
            .cmp(&a.score_millis)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    matches.truncate(query.limit.clamp(1, 200));
    Ok(matches)
}

fn git_revision(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn workspace_fingerprint(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .ok()?;
    if !output.status.success() {
        return filesystem_fingerprint(root);
    }
    let mut hasher = Sha256::new();
    hasher.update(git_revision(root).unwrap_or_default().as_bytes());
    hasher.update(&output.stdout);
    Some(format!("{:x}", hasher.finalize()))
}

fn filesystem_fingerprint(root: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    for entry in WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
    {
        let entry = entry.ok()?;
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || language_for(entry.path()).is_none()
        {
            continue;
        }
        let metadata = entry.metadata().ok()?;
        let relative = entry.path().strip_prefix(root).ok()?;
        hasher.update(relative.as_os_str().as_encoded_bytes());
        hasher.update(metadata.len().to_le_bytes());
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        hasher.update(modified.to_le_bytes());
    }
    Some(format!("filesystem:{:x}", hasher.finalize()))
}

fn persisted_files_are_current(root: &Path, files: &[IndexedFile]) -> bool {
    files.iter().all(|file| {
        let Ok(metadata) = fs::symlink_metadata(root.join(&file.path)) else {
            return false;
        };
        if !metadata.file_type().is_file() || metadata.len() != file.len {
            return false;
        }
        let modified_millis = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        modified_millis == file.modified_millis
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_search_persists_and_invalidation_updates_results() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("lib.rs"),
            "pub struct DomesticModelRuntime;\nimpl DomesticModelRuntime {\n  fn resolve_capability() {}\n}\n",
        )
        .unwrap();
        let index = ProjectCodeIndex::new(root.path(), cache.path().join("index.json")).unwrap();
        let matches = index
            .search(&CodeQuery {
                text: "resolve capability".to_string(),
                path_prefix: None,
                language: Some("rust".to_string()),
                limit: 10,
            })
            .unwrap();
        assert!(!matches.is_empty());
        assert_eq!(matches[0].symbol.as_deref(), Some("resolve_capability"));
        assert!(index.status().ready);
        assert!(cache.path().join("index.json").exists());

        fs::write(root.path().join("lib.rs"), "pub fn new_symbol() {}\n").unwrap();
        index.invalidate(&[PathBuf::from("lib.rs")]).unwrap();
        let matches = index
            .search(&CodeQuery {
                text: "new_symbol".to_string(),
                path_prefix: None,
                language: None,
                limit: 10,
            })
            .unwrap();
        assert_eq!(matches[0].symbol.as_deref(), Some("new_symbol"));
    }

    #[test]
    fn invalid_cache_is_rebuilt_and_path_escape_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("main.py"),
            "def route_model():\n    pass\n",
        )
        .unwrap();
        let path = cache.path().join("index.json");
        fs::write(&path, "not-json").unwrap();
        let index = ProjectCodeIndex::new(root.path(), &path).unwrap();
        assert!(!index
            .search(&CodeQuery {
                text: "route model".to_string(),
                path_prefix: None,
                language: None,
                limit: 5,
            })
            .unwrap()
            .is_empty());
        assert!(index
            .search(&CodeQuery {
                text: "route".to_string(),
                path_prefix: Some(PathBuf::from("../outside")),
                language: None,
                limit: 5,
            })
            .is_err());
    }

    #[test]
    fn persisted_index_is_rebuilt_when_a_still_dirty_file_changes_again() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let path = root.path().join("lib.rs");
        let cache_path = cache.path().join("index.json");
        fs::write(&path, "pub fn first_symbol() {}\n").unwrap();
        {
            let index = ProjectCodeIndex::new(root.path(), &cache_path).unwrap();
            assert!(!index
                .search(&CodeQuery {
                    text: "first_symbol".to_string(),
                    path_prefix: None,
                    language: Some("rust".to_string()),
                    limit: 5,
                })
                .unwrap()
                .is_empty());
        }
        fs::write(&path, "pub fn second_symbol_with_new_length() {}\n").unwrap();
        let index = ProjectCodeIndex::new(root.path(), &cache_path).unwrap();
        let matches = index
            .search(&CodeQuery {
                text: "second_symbol_with_new_length".to_string(),
                path_prefix: None,
                language: Some("rust".to_string()),
                limit: 5,
            })
            .unwrap();
        assert_eq!(
            matches[0].symbol.as_deref(),
            Some("second_symbol_with_new_length")
        );
    }
}
