//! Runtime activation for installed plugin hooks, styles, themes, channels, LSP servers,
//! monitors, settings, and user configuration.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, Command as StdCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use jsonschema::{Draft, JSONSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;

use crate::lockfile;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTextAsset {
    pub plugin: String,
    pub name: String,
    pub content: String,
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTheme {
    pub plugin: String,
    pub name: String,
    pub palette: Value,
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginProcessSpec {
    pub plugin: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub auto_start: bool,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginRuntimeCatalog {
    #[serde(skip)]
    pub hooks: wyj_core::HooksSettings,
    pub output_styles: BTreeMap<String, PluginTextAsset>,
    pub themes: BTreeMap<String, PluginTheme>,
    pub channels: BTreeMap<String, PluginProcessSpec>,
    pub lsp_servers: BTreeMap<String, PluginProcessSpec>,
    pub monitors: BTreeMap<String, PluginProcessSpec>,
    pub settings_schema: BTreeMap<String, Value>,
    pub user_config: BTreeMap<String, Value>,
    pub warnings: Vec<String>,
}

impl PluginRuntimeCatalog {
    pub fn load(cwd: &Path) -> Self {
        let mut catalog = Self::default();
        for entry in lockfile::enabled_plugin_entries(cwd) {
            match catalog.activate_entry_transactionally(&entry) {
                Ok(()) => {}
                Err(error) => catalog
                    .warnings
                    .push(format!("plugin {} runtime: {error}", entry.name)),
            }
        }
        catalog
    }

    pub fn load_with_local(
        cwd: &Path,
        local: Option<(&str, &Path, &lockfile::PluginContributions)>,
    ) -> Self {
        let mut catalog = Self::load(cwd);
        if let Some((name, root, contributions)) = local {
            let entry = lockfile::InstalledPluginEntry {
                name: name.to_string(),
                version: None,
                scope: lockfile::InstallScope::Project,
                source: lockfile::PluginInstallOrigin::Local {
                    path: root.to_path_buf(),
                },
                enabled: true,
                installed_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                plugin_root: root.to_path_buf(),
                contributes: contributions.clone(),
            };
            match catalog.activate_entry_transactionally(&entry) {
                Ok(()) => {}
                Err(error) => catalog
                    .warnings
                    .push(format!("plugin {name} runtime: {error}")),
            }
        }
        catalog
    }

    fn activate_entry_transactionally(
        &mut self,
        entry: &lockfile::InstalledPluginEntry,
    ) -> Result<()> {
        let mut staged = self.clone();
        staged.activate_entry(entry)?;
        *self = staged;
        Ok(())
    }

    fn activate_entry(&mut self, entry: &lockfile::InstalledPluginEntry) -> Result<()> {
        let root = std::fs::canonicalize(&entry.plugin_root)
            .with_context(|| format!("canonicalize plugin root {}", entry.plugin_root.display()))?;
        let runtime = &entry.contributes.runtime;
        if let Some(raw) = &runtime.hooks {
            let settings = load_json_value(&root, raw).and_then(parse_hooks)?;
            merge_hooks(&mut self.hooks, settings);
        }
        if let Some(raw) = &runtime.output_styles {
            for asset in load_text_assets(&entry.name, &root, raw)? {
                insert_unique(
                    &mut self.output_styles,
                    asset.name.clone(),
                    asset,
                    &entry.name,
                    "output style",
                    &mut self.warnings,
                );
            }
        }
        if let Some(raw) = &runtime.themes {
            for theme in load_themes(&entry.name, &root, raw)? {
                insert_unique(
                    &mut self.themes,
                    theme.name.clone(),
                    theme,
                    &entry.name,
                    "theme",
                    &mut self.warnings,
                );
            }
        }
        for (raw, target, kind) in [
            (&runtime.channels, &mut self.channels, "channel"),
            (&runtime.lsp_servers, &mut self.lsp_servers, "lsp server"),
            (&runtime.monitors, &mut self.monitors, "monitor"),
        ] {
            if let Some(raw) = raw {
                for spec in load_process_specs(&entry.name, &root, raw)? {
                    insert_unique(
                        target,
                        spec.name.clone(),
                        spec,
                        &entry.name,
                        kind,
                        &mut self.warnings,
                    );
                }
            }
        }
        if let Some(raw) = &runtime.settings {
            let schema = load_json_value(&root, raw)?;
            {
                JSONSchema::options()
                    .with_draft(Draft::Draft7)
                    .compile(&schema)
                    .map_err(|error| anyhow::anyhow!("invalid settings schema: {error}"))?;
            }
            self.settings_schema.insert(entry.name.clone(), schema);
        }
        if let Some(raw) = &runtime.user_config {
            let config = load_json_value(&root, raw)?;
            if let Some(schema) = self.settings_schema.get(&entry.name) {
                let compiled = JSONSchema::options()
                    .with_draft(Draft::Draft7)
                    .compile(schema)
                    .map_err(|error| anyhow::anyhow!("invalid settings schema: {error}"))?;
                if let Err(errors) = compiled.validate(&config) {
                    let messages = errors.map(|error| error.to_string()).collect::<Vec<_>>();
                    bail!(
                        "userConfig does not match settings schema: {}",
                        messages.join("; ")
                    )
                };
            }
            self.user_config.insert(entry.name.clone(), config);
        }
        Ok(())
    }

    pub fn active_output_style(&self) -> Option<&PluginTextAsset> {
        let selected = selected_name(&self.user_config, "activeOutputStyle")
            .or_else(|| std::env::var("WYJ_CODE_OUTPUT_STYLE").ok());
        selected
            .as_deref()
            .and_then(|name| self.output_styles.get(name))
    }

    pub fn active_theme(&self) -> Option<&PluginTheme> {
        let selected = selected_name(&self.user_config, "activeTheme")
            .or_else(|| std::env::var("WYJ_CODE_THEME").ok());
        selected.as_deref().and_then(|name| self.themes.get(name))
    }

    pub async fn emit_channel_event(&self, event: &str, payload: &Value) -> Vec<ChannelResult> {
        let mut results = Vec::new();
        for spec in self.channels.values().filter(|spec| {
            spec.events.is_empty()
                || spec
                    .events
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(event))
        }) {
            results.push(run_channel(spec, event, payload).await);
        }
        results
    }

    pub fn start_lsp(&self, name: &str) -> Result<Child> {
        let spec = self
            .lsp_servers
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown plugin LSP server: {name}"))?;
        spawn_process(spec, true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelResult {
    pub name: String,
    pub success: bool,
    pub output: String,
}

pub struct PluginProcessSupervisor {
    children: Mutex<Vec<(String, Child)>>,
    lsp_clients: Mutex<HashMap<String, LspClient>>,
}

impl PluginProcessSupervisor {
    pub fn start(catalog: &PluginRuntimeCatalog) -> Self {
        let mut children = Vec::new();
        for spec in catalog.monitors.values().filter(|spec| spec.auto_start) {
            match spawn_process(spec, false) {
                Ok(child) => children.push((spec.name.clone(), child)),
                Err(error) => {
                    tracing::warn!("plugin monitor {} failed to start: {error}", spec.name)
                }
            }
        }
        Self {
            children: Mutex::new(children),
            lsp_clients: Mutex::new(HashMap::new()),
        }
    }

    /// Lazily start every plugin LSP server that declares support for `language`. Starting an
    /// already-running server is idempotent. Failed servers remain retryable on a later search.
    pub fn ensure_lsp_for_language(
        &self,
        catalog: &PluginRuntimeCatalog,
        language: &str,
    ) -> Vec<String> {
        let mut clients = self.lsp_clients.lock().unwrap();
        clients.retain(|_, client| client.child.try_wait().is_ok_and(|status| status.is_none()));
        let mut started = Vec::new();
        for spec in catalog.lsp_servers.values().filter(|spec| {
            spec.languages.is_empty()
                || spec
                    .languages
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(language))
        }) {
            if clients.contains_key(&spec.name) {
                continue;
            }
            match LspClient::start(spec) {
                Ok(client) => {
                    started.push(spec.name.clone());
                    clients.insert(spec.name.clone(), client);
                }
                Err(error) => tracing::warn!("plugin LSP {} failed to start: {error}", spec.name),
            }
        }
        started
    }

    fn query_lsp_symbols(
        &self,
        catalog: &PluginRuntimeCatalog,
        query: &wyj_core::CodeQuery,
    ) -> Vec<wyj_core::CodeMatch> {
        let Some(language) = query.language.as_deref() else {
            return Vec::new();
        };
        self.ensure_lsp_for_language(catalog, language);
        let names: Vec<String> = catalog
            .lsp_servers
            .values()
            .filter(|spec| {
                spec.languages.is_empty()
                    || spec
                        .languages
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(language))
            })
            .map(|spec| spec.name.clone())
            .collect();
        let mut clients = self.lsp_clients.lock().unwrap();
        let mut failed = Vec::new();
        let mut matches = Vec::new();
        for name in names {
            let Some(client) = clients.get_mut(&name) else {
                continue;
            };
            match client.workspace_symbols(query) {
                Ok(found) => matches.extend(found),
                Err(error) => {
                    tracing::warn!("plugin LSP {name} query failed: {error}");
                    failed.push(name);
                }
            }
        }
        for name in failed {
            if let Some(mut client) = clients.remove(&name) {
                client.shutdown();
            }
        }
        matches
    }

    pub fn running_names(&self) -> Vec<String> {
        let mut children = self.children.lock().unwrap();
        let mut names: Vec<String> = children
            .iter_mut()
            .filter_map(|(name, child)| match child.try_wait() {
                Ok(None) => Some(name.clone()),
                _ => None,
            })
            .collect();
        let mut clients = self.lsp_clients.lock().unwrap();
        clients.retain(|_, client| client.child.try_wait().is_ok_and(|status| status.is_none()));
        names.extend(clients.keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    pub fn shutdown(&self) {
        let mut children = self.children.lock().unwrap();
        for (_, child) in children.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        children.clear();
        let mut clients = self.lsp_clients.lock().unwrap();
        for client in clients.values_mut() {
            client.shutdown();
        }
        clients.clear();
    }
}

impl Drop for PluginProcessSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    messages: std::sync::mpsc::Receiver<std::result::Result<Value, String>>,
    next_id: u64,
    initialized: bool,
    root: PathBuf,
}

impl LspClient {
    fn start(spec: &PluginProcessSpec) -> Result<Self> {
        let mut child = spawn_process(spec, true)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("plugin LSP {} has no stdin", spec.name))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("plugin LSP {} has no stdout", spec.name))?;
        if let Some(mut stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = stderr.read_to_end(&mut sink);
            });
        }
        let (tx, messages) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_lsp_message(&mut reader) {
                    Ok(Some(value)) => {
                        if tx.send(Ok(value)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = tx.send(Err(error.to_string()));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            messages,
            next_id: 1,
            initialized: false,
            root: std::fs::canonicalize(&spec.cwd).unwrap_or_else(|_| spec.cwd.clone()),
        })
    }

    fn workspace_symbols(
        &mut self,
        query: &wyj_core::CodeQuery,
    ) -> Result<Vec<wyj_core::CodeMatch>> {
        self.ensure_initialized()?;
        let result = self.request(
            "workspace/symbol",
            json!({"query": query.text}),
            Duration::from_secs(3),
        )?;
        parse_lsp_symbols(&self.root, result, query)
    }

    fn ensure_initialized(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        let root_uri = url::Url::from_directory_path(&self.root)
            .map_err(|_| anyhow::anyhow!("cannot convert LSP root to file URI"))?;
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri.as_str(),
                "capabilities": {"workspace": {"symbol": {"dynamicRegistration": false}}},
                "clientInfo": {"name": "wyj-code", "version": env!("CARGO_PKG_VERSION")}
            }),
            Duration::from_secs(5),
        )?;
        self.notify("initialized", json!({}))?;
        self.initialized = true;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        loop {
            let message = self
                .messages
                .recv_timeout(timeout)
                .map_err(|error| anyhow::anyhow!("LSP {method} response timeout: {error}"))?
                .map_err(anyhow::Error::msg)?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                bail!("LSP {method} error: {error}")
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc":"2.0","method":method,"params":params}))
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn shutdown(&mut self) {
        if self.initialized {
            let _ = self.request("shutdown", Value::Null, Duration::from_millis(300));
            let _ = self.notify("exit", Value::Null);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_lsp_message(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let length = content_length.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing LSP Content-Length",
        )
    })?;
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn parse_lsp_symbols(
    root: &Path,
    result: Value,
    query: &wyj_core::CodeQuery,
) -> Result<Vec<wyj_core::CodeMatch>> {
    let values = result.as_array().cloned().unwrap_or_default();
    let prefix = query.path_prefix.as_ref().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        }
    });
    let mut matches = Vec::new();
    for value in values {
        let Some(name) = value.get("name").and_then(Value::as_str) else {
            continue;
        };
        let location = value.get("location").unwrap_or(&value);
        let Some(uri) = location.get("uri").and_then(Value::as_str) else {
            continue;
        };
        let Ok(url) = url::Url::parse(uri) else {
            continue;
        };
        let Ok(path) = url.to_file_path() else {
            continue;
        };
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        if !canonical.starts_with(root)
            || prefix.as_ref().is_some_and(|p| !canonical.starts_with(p))
        {
            continue;
        }
        let relative = canonical
            .strip_prefix(root)
            .unwrap_or(&canonical)
            .to_path_buf();
        let line = location
            .pointer("/range/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1) as u32;
        let container = value.get("containerName").and_then(Value::as_str);
        matches.push(wyj_core::CodeMatch {
            path: relative,
            line,
            symbol: Some(name.to_string()),
            kind: value
                .get("kind")
                .and_then(Value::as_u64)
                .map(lsp_symbol_kind)
                .map(str::to_string),
            snippet: container
                .map(|container| format!("{container}::{name}"))
                .unwrap_or_else(|| name.to_string()),
            score_millis: 10_000,
        });
    }
    Ok(matches)
}

fn lsp_symbol_kind(kind: u64) -> &'static str {
    match kind {
        5 => "class",
        6 => "method",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        22 => "enum_member",
        23 => "struct",
        _ => "symbol",
    }
}

/// CodeIndex decorator that merges real `workspace/symbol` responses from plugin LSP servers
/// with the local lexical/symbol index. LSP startup, protocol or query failures are fail-soft:
/// the local index and direct-scan fallback remain available.
pub struct PluginCodeIndex {
    inner: Arc<dyn wyj_core::CodeIndex>,
    catalog: Arc<PluginRuntimeCatalog>,
    processes: Arc<PluginProcessSupervisor>,
}

impl PluginCodeIndex {
    pub fn new(
        inner: Arc<dyn wyj_core::CodeIndex>,
        catalog: Arc<PluginRuntimeCatalog>,
        processes: Arc<PluginProcessSupervisor>,
    ) -> Self {
        Self {
            inner,
            catalog,
            processes,
        }
    }
}

impl wyj_core::CodeIndex for PluginCodeIndex {
    fn status(&self) -> wyj_core::CodeIndexStatus {
        let mut status = self.inner.status();
        if !self.catalog.lsp_servers.is_empty() {
            status.backend = format!(
                "{}+plugin-lsp({})",
                status.backend,
                self.catalog.lsp_servers.len()
            );
        }
        status
    }

    fn search(&self, query: &wyj_core::CodeQuery) -> Result<Vec<wyj_core::CodeMatch>> {
        let mut matches = self.inner.search(query)?;
        matches.extend(self.processes.query_lsp_symbols(&self.catalog, query));
        let mut seen = HashSet::new();
        matches.retain(|item| seen.insert((item.path.clone(), item.line, item.symbol.clone())));
        matches.sort_by(|left, right| {
            right
                .score_millis
                .cmp(&left.score_millis)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.line.cmp(&right.line))
        });
        if query.limit > 0 {
            matches.truncate(query.limit);
        }
        Ok(matches)
    }

    fn invalidate(&self, paths: &[PathBuf]) -> Result<()> {
        self.inner.invalidate(paths)
    }
}

fn parse_hooks(value: Value) -> Result<wyj_core::HooksSettings> {
    let hooks = value.get("hooks").cloned().unwrap_or(value);
    serde_json::from_value(json!({"hooks": hooks})).context("parse plugin hooks")
}

fn merge_hooks(target: &mut wyj_core::HooksSettings, incoming: wyj_core::HooksSettings) {
    target.append(incoming);
}

fn load_json_value(root: &Path, value: &Value) -> Result<Value> {
    match value {
        Value::String(path) => {
            let path = safe_plugin_path(root, path)?;
            serde_json::from_slice(&std::fs::read(&path)?)
                .with_context(|| format!("parse JSON {}", path.display()))
        }
        value => Ok(value.clone()),
    }
}

fn load_text_assets(plugin: &str, root: &Path, raw: &Value) -> Result<Vec<PluginTextAsset>> {
    let mut assets = Vec::new();
    match raw {
        Value::String(value) => assets.push(text_asset(plugin, root, None, value)?),
        Value::Array(values) => {
            for value in values {
                match value {
                    Value::String(value) => assets.push(text_asset(plugin, root, None, value)?),
                    Value::Object(object) => {
                        let name = object.get("name").and_then(Value::as_str);
                        let value = object
                            .get("path")
                            .or_else(|| object.get("content"))
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("text asset needs path or content"))?;
                        assets.push(text_asset(plugin, root, name, value)?);
                    }
                    _ => bail!("invalid text asset entry"),
                }
            }
        }
        Value::Object(object) => {
            for (name, value) in object {
                let value = value
                    .as_str()
                    .or_else(|| value.get("path").and_then(Value::as_str))
                    .or_else(|| value.get("content").and_then(Value::as_str))
                    .ok_or_else(|| anyhow::anyhow!("text asset {name} needs path or content"))?;
                assets.push(text_asset(plugin, root, Some(name), value)?);
            }
        }
        _ => bail!("invalid text assets field"),
    }
    Ok(assets)
}

fn text_asset(
    plugin: &str,
    root: &Path,
    name: Option<&str>,
    value: &str,
) -> Result<PluginTextAsset> {
    let candidate = safe_plugin_path(root, value);
    let (content, source, inferred) = match candidate {
        Ok(path) if path.is_file() => {
            let inferred = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("style")
                .to_string();
            (std::fs::read_to_string(&path)?, Some(path), inferred)
        }
        _ => (value.to_string(), None, "style".to_string()),
    };
    Ok(PluginTextAsset {
        plugin: plugin.to_string(),
        name: name.unwrap_or(&inferred).to_string(),
        content,
        source,
    })
}

fn load_themes(plugin: &str, root: &Path, raw: &Value) -> Result<Vec<PluginTheme>> {
    let value = load_json_value(root, raw)?;
    let mut themes = Vec::new();
    match value {
        Value::Array(values) => {
            for value in values {
                let path = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("theme array entries must be paths"))?;
                let source = safe_plugin_path(root, path)?;
                let palette: Value = serde_json::from_slice(&std::fs::read(&source)?)?;
                validate_theme_palette(&palette)?;
                let name = source
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("theme")
                    .to_string();
                themes.push(PluginTheme {
                    plugin: plugin.to_string(),
                    name,
                    palette,
                    source: Some(source),
                });
            }
        }
        Value::Object(object) => {
            for (name, palette) in object {
                let (palette, source) = if let Some(path) = palette.as_str() {
                    let source = safe_plugin_path(root, path)?;
                    (
                        serde_json::from_slice(&std::fs::read(&source)?)?,
                        Some(source),
                    )
                } else {
                    (palette, None)
                };
                validate_theme_palette(&palette)?;
                themes.push(PluginTheme {
                    plugin: plugin.to_string(),
                    name,
                    palette,
                    source,
                });
            }
        }
        _ => bail!("themes must be an object or path array"),
    }
    Ok(themes)
}

fn validate_theme_palette(palette: &Value) -> Result<()> {
    if !palette.is_object() {
        bail!("theme palette must be a JSON object")
    }
    Ok(())
}

fn load_process_specs(plugin: &str, root: &Path, raw: &Value) -> Result<Vec<PluginProcessSpec>> {
    let value = load_json_value(root, raw)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("process capability must be an object"))?;
    let mut specs = Vec::new();
    for (name, value) in object {
        let (command, args, env, events, languages, auto_start) = match value {
            Value::String(command) => (
                command.clone(),
                Vec::new(),
                HashMap::new(),
                Vec::new(),
                Vec::new(),
                false,
            ),
            Value::Object(object) => (
                object
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("process {name} needs command"))?
                    .to_string(),
                string_array(object.get("args"))?,
                string_map(object.get("env"))?,
                string_array(object.get("events"))?,
                string_array(object.get("languages"))?,
                object
                    .get("autoStart")
                    .or_else(|| object.get("auto_start"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            _ => bail!("invalid process spec {name}"),
        };
        specs.push(PluginProcessSpec {
            plugin: plugin.to_string(),
            name: name.clone(),
            command,
            args,
            env,
            events,
            languages,
            auto_start,
            cwd: root.to_path_buf(),
        });
    }
    Ok(specs)
}

fn string_array(value: Option<&Value>) -> Result<Vec<String>> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("expected string array"))
            })
            .collect(),
        _ => bail!("expected string array"),
    }
}

fn string_map(value: Option<&Value>) -> Result<HashMap<String, String>> {
    match value {
        None => Ok(HashMap::new()),
        Some(Value::Object(values)) => values
            .iter()
            .map(|(key, value)| {
                Ok((
                    key.clone(),
                    value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("expected string env value"))?
                        .to_string(),
                ))
            })
            .collect(),
        _ => bail!("expected env object"),
    }
}

fn safe_plugin_path(root: &Path, raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if path.is_absolute() {
        bail!("plugin capability path must be relative: {raw}")
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            bail!("plugin capability path escapes plugin root: {raw}")
        }
    }
    let joined = root.join(path);
    let canonical = std::fs::canonicalize(&joined)
        .with_context(|| format!("resolve plugin capability path {}", joined.display()))?;
    if !canonical.starts_with(root) {
        bail!("plugin capability symlink escapes plugin root: {raw}")
    }
    Ok(canonical)
}

fn insert_unique<T>(
    target: &mut BTreeMap<String, T>,
    name: String,
    value: T,
    plugin: &str,
    kind: &str,
    warnings: &mut Vec<String>,
) {
    match target.entry(name) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(value);
        }
        std::collections::btree_map::Entry::Occupied(entry) => {
            warnings.push(format!(
                "plugin {plugin} {kind} `{}` conflicts with an earlier plugin",
                entry.key()
            ));
        }
    }
}

fn selected_name(config: &BTreeMap<String, Value>, key: &str) -> Option<String> {
    config
        .values()
        .find_map(|value| value.get(key).and_then(Value::as_str).map(str::to_string))
}

fn spawn_process(spec: &PluginProcessSpec, piped: bool) -> Result<Child> {
    let mut command = StdCommand::new(&spec.command);
    command
        .args(&spec.args)
        .envs(&spec.env)
        .current_dir(&spec.cwd);
    if piped {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    command
        .spawn()
        .with_context(|| format!("start plugin process {}", spec.name))
}

async fn run_channel(spec: &PluginProcessSpec, event: &str, payload: &Value) -> ChannelResult {
    run_channel_with_timeout(spec, event, payload, std::time::Duration::from_secs(30)).await
}

async fn run_channel_with_timeout(
    spec: &PluginProcessSpec,
    event: &str,
    payload: &Value,
    timeout: std::time::Duration,
) -> ChannelResult {
    let mut command = TokioCommand::new(&spec.command);
    command
        .args(&spec.args)
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let result = async {
        let mut child = command.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(
                    serde_json::to_string(&json!({"event": event, "payload": payload}))?.as_bytes(),
                )
                .await?;
        }
        child.wait_with_output().await
    };
    match tokio::time::timeout(timeout, result).await {
        Ok(Ok(output)) => ChannelResult {
            name: spec.name.clone(),
            success: output.status.success(),
            output: if output.status.success() {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            } else {
                String::from_utf8_lossy(&output.stderr).trim().to_string()
            },
        },
        Ok(Err(error)) => ChannelResult {
            name: spec.name.clone(),
            success: false,
            output: error.to_string(),
        },
        Err(_) => ChannelResult {
            name: spec.name.clone(),
            success: false,
            output: "channel command timed out".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::PluginRuntimeContributions;

    fn entry(root: &Path) -> lockfile::InstalledPluginEntry {
        lockfile::InstalledPluginEntry {
            name: "runtime-plugin".to_string(),
            version: Some("1.0.0".to_string()),
            scope: lockfile::InstallScope::Project,
            source: lockfile::PluginInstallOrigin::Local {
                path: root.to_path_buf(),
            },
            enabled: true,
            installed_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            plugin_root: root.to_path_buf(),
            contributes: lockfile::PluginContributions {
                skill_paths: Vec::new(),
                agent_paths: Vec::new(),
                mcp_servers: Vec::new(),
                runtime: PluginRuntimeContributions {
                    hooks: Some(json!({"PreToolUse": [{"matcher":"Bash","hooks":[]}]})),
                    output_styles: Some(json!({"concise": "styles/concise.md"})),
                    themes: Some(json!({"warm": {"claude": "#d77757"}})),
                    channels: Some(
                        json!({"audit": {"command": "printf", "args": ["ok"], "events": ["turn_finished"]}}),
                    ),
                    lsp_servers: Some(
                        json!({"rust": {"command": "rust-analyzer", "languages": ["rust"]}}),
                    ),
                    monitors: Some(json!({"watch": {"command": "true", "autoStart": false}})),
                    settings: Some(
                        json!({"type":"object","properties":{"activeTheme":{"type":"string"},"activeOutputStyle":{"type":"string"}}}),
                    ),
                    user_config: Some(json!({"activeTheme":"warm","activeOutputStyle":"concise"})),
                },
                skipped_capabilities: Vec::new(),
            },
        }
    }

    #[test]
    fn activates_all_runtime_capability_families() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("styles")).unwrap();
        std::fs::write(root.path().join("styles/concise.md"), "Be concise.").unwrap();
        let mut catalog = PluginRuntimeCatalog::default();
        catalog.activate_entry(&entry(root.path())).unwrap();
        assert_eq!(catalog.hooks.hooks["PreToolUse"].len(), 1);
        assert_eq!(catalog.output_styles["concise"].content, "Be concise.");
        assert_eq!(catalog.active_output_style().unwrap().name, "concise");
        assert_eq!(catalog.active_theme().unwrap().name, "warm");
        assert!(catalog.channels.contains_key("audit"));
        assert!(catalog.lsp_servers.contains_key("rust"));
        assert!(catalog.monitors.contains_key("watch"));
        assert!(catalog.warnings.is_empty());
    }

    #[test]
    fn rejects_plugin_path_and_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        assert!(safe_plugin_path(root.path(), "../outside.json").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_plugin_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), root.path().join("escape.json")).unwrap();
        assert!(safe_plugin_path(root.path(), "escape.json").is_err());
    }

    #[test]
    fn invalid_settings_schema_and_user_config_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("styles")).unwrap();
        std::fs::write(root.path().join("styles/concise.md"), "Be concise.").unwrap();

        let mut invalid_schema = entry(root.path());
        invalid_schema.contributes.runtime.settings = Some(json!({"type": 7}));
        let error = PluginRuntimeCatalog::default()
            .activate_entry(&invalid_schema)
            .unwrap_err();
        assert!(error.to_string().contains("invalid settings schema"));

        let mut invalid_config = entry(root.path());
        invalid_config.contributes.runtime.user_config = Some(json!({"activeTheme": 42}));
        let error = PluginRuntimeCatalog::default()
            .activate_entry(&invalid_config)
            .unwrap_err();
        assert!(error.to_string().contains("userConfig does not match"));
    }

    #[test]
    fn invalid_theme_palette_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let error = load_themes("test", root.path(), &json!({"broken": 42})).unwrap_err();
        assert!(error.to_string().contains("JSON object"));
    }

    #[test]
    fn invalid_plugin_runtime_activation_is_transactional() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("styles")).unwrap();
        std::fs::write(root.path().join("styles/concise.md"), "Be concise.").unwrap();
        let mut broken = entry(root.path());
        broken.contributes.runtime.themes = Some(json!({"broken": 42}));

        let mut catalog = PluginRuntimeCatalog::default();
        let error = catalog.activate_entry_transactionally(&broken).unwrap_err();
        assert!(error.to_string().contains("JSON object"));
        assert!(catalog.hooks.hooks.is_empty());
        assert!(catalog.output_styles.is_empty());
        assert!(catalog.themes.is_empty());
        assert!(catalog.channels.is_empty());
        assert!(catalog.lsp_servers.is_empty());
        assert!(catalog.monitors.is_empty());
        assert!(catalog.settings_schema.is_empty());
        assert!(catalog.user_config.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn channel_receives_structured_event_payload() {
        let root = tempfile::tempdir().unwrap();
        let spec = PluginProcessSpec {
            plugin: "test".to_string(),
            name: "channel".to_string(),
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "read value; printf received".to_string()],
            env: HashMap::new(),
            events: vec!["done".to_string()],
            languages: Vec::new(),
            auto_start: false,
            cwd: root.path().to_path_buf(),
        };
        let result = run_channel(&spec, "done", &json!({"ok":true})).await;
        assert!(result.success);
        assert_eq!(result.output, "received");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn channel_reports_nonzero_exit_and_timeout() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = PluginProcessSpec {
            plugin: "test".to_string(),
            name: "channel".to_string(),
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "read value; printf failed >&2; exit 7".to_string(),
            ],
            env: HashMap::new(),
            events: Vec::new(),
            languages: Vec::new(),
            auto_start: false,
            cwd: root.path().to_path_buf(),
        };
        let failed = run_channel(&spec, "done", &json!({"ok":true})).await;
        assert!(!failed.success);
        assert_eq!(failed.output, "failed");

        spec.args = vec!["-c".to_string(), "read value; sleep 1".to_string()];
        let timed_out = run_channel_with_timeout(
            &spec,
            "done",
            &json!({"ok":true}),
            std::time::Duration::from_millis(20),
        )
        .await;
        assert!(!timed_out.success);
        assert!(timed_out.output.contains("timed out"));
    }

    #[test]
    fn monitor_start_failure_is_nonfatal() {
        let root = tempfile::tempdir().unwrap();
        let mut catalog = PluginRuntimeCatalog::default();
        catalog.monitors.insert(
            "missing".to_string(),
            PluginProcessSpec {
                plugin: "test".to_string(),
                name: "missing".to_string(),
                command: root.path().join("does-not-exist").display().to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                events: Vec::new(),
                languages: Vec::new(),
                auto_start: true,
                cwd: root.path().to_path_buf(),
            },
        );
        let supervisor = PluginProcessSupervisor::start(&catalog);
        assert!(supervisor.running_names().is_empty());
    }

    struct FakeIndex {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl wyj_core::CodeIndex for FakeIndex {
        fn status(&self) -> wyj_core::CodeIndexStatus {
            wyj_core::CodeIndexStatus {
                backend: "fake".to_string(),
                ready: true,
                indexed_files: 1,
                revision: None,
                fallback_available: true,
            }
        }

        fn search(&self, _query: &wyj_core::CodeQuery) -> Result<Vec<wyj_core::CodeMatch>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![wyj_core::CodeMatch {
                path: PathBuf::from("lib.rs"),
                line: 1,
                symbol: Some("found".to_string()),
                kind: Some("function".to_string()),
                snippet: "fn found() {}".to_string(),
                score_millis: 1,
            }])
        }

        fn invalidate(&self, _paths: &[PathBuf]) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn lsp_start_failure_never_breaks_code_search_fallback() {
        let root = tempfile::tempdir().unwrap();
        let mut catalog = PluginRuntimeCatalog::default();
        catalog.lsp_servers.insert(
            "missing".to_string(),
            PluginProcessSpec {
                plugin: "test".to_string(),
                name: "missing".to_string(),
                command: root.path().join("does-not-exist").display().to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                events: Vec::new(),
                languages: vec!["rust".to_string()],
                auto_start: false,
                cwd: root.path().to_path_buf(),
            },
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let processes = Arc::new(PluginProcessSupervisor::start(&catalog));
        let index = PluginCodeIndex::new(
            Arc::new(FakeIndex {
                calls: calls.clone(),
            }),
            Arc::new(catalog),
            processes,
        );
        let matches = wyj_core::CodeIndex::search(
            &index,
            &wyj_core::CodeQuery {
                text: "found".to_string(),
                path_prefix: None,
                language: Some("rust".to_string()),
                limit: 5,
            },
        )
        .unwrap();
        assert_eq!(matches[0].symbol.as_deref(), Some("found"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn plugin_lsp_workspace_symbols_are_merged_with_local_results() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.rs");
        std::fs::write(&source, "fn lsp_found() {}\n").unwrap();
        let uri = url::Url::from_file_path(&source).unwrap();
        let symbol_response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": [{
                "name": "lsp_found",
                "kind": 12,
                "location": {
                    "uri": uri.as_str(),
                    "range": {
                        "start": {"line": 0, "character": 3},
                        "end": {"line": 0, "character": 12}
                    }
                }
            }]
        })
        .to_string();
        let script = format!(
            r#"
send() {{
  body="$1"
  length=$(printf '%s' "$body" | wc -c | tr -d ' ')
  printf 'Content-Length: %s\r\n\r\n%s' "$length" "$body"
}}
while :; do
  length=""
  while IFS= read -r line; do
    line=$(printf '%s' "$line" | tr -d '\r')
    [ -z "$line" ] && break
    case "$line" in Content-Length:*) length=${{line#Content-Length: }} ;; esac
  done
  [ -z "$length" ] && exit 0
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in
    *'"method":"initialize"'*) send '{{"jsonrpc":"2.0","id":1,"result":{{"capabilities":{{"workspaceSymbolProvider":true}}}}}}' ;;
    *'"method":"workspace/symbol"'*) send '{}' ;;
  esac
done
"#,
            symbol_response
        );
        let mut catalog = PluginRuntimeCatalog::default();
        catalog.lsp_servers.insert(
            "fake-rust".to_string(),
            PluginProcessSpec {
                plugin: "test".to_string(),
                name: "fake-rust".to_string(),
                command: "sh".to_string(),
                args: vec!["-c".to_string(), script],
                env: HashMap::new(),
                events: Vec::new(),
                languages: vec!["rust".to_string()],
                auto_start: false,
                cwd: root.path().to_path_buf(),
            },
        );
        let processes = Arc::new(PluginProcessSupervisor::start(&catalog));
        let index = PluginCodeIndex::new(
            Arc::new(FakeIndex {
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Arc::new(catalog),
            processes,
        );
        let matches = wyj_core::CodeIndex::search(
            &index,
            &wyj_core::CodeQuery {
                text: "lsp_found".to_string(),
                path_prefix: None,
                language: Some("rust".to_string()),
                limit: 5,
            },
        )
        .unwrap();
        assert_eq!(matches[0].symbol.as_deref(), Some("lsp_found"));
        assert_eq!(matches[0].kind.as_deref(), Some("function"));
        assert_eq!(matches[0].path, PathBuf::from("lib.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn lsp_start_is_idempotent_for_the_same_language() {
        let root = tempfile::tempdir().unwrap();
        let mut catalog = PluginRuntimeCatalog::default();
        catalog.lsp_servers.insert(
            "rust".to_string(),
            PluginProcessSpec {
                plugin: "test".to_string(),
                name: "rust".to_string(),
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "sleep 5".to_string()],
                env: HashMap::new(),
                events: Vec::new(),
                languages: vec!["rust".to_string()],
                auto_start: false,
                cwd: root.path().to_path_buf(),
            },
        );
        let supervisor = PluginProcessSupervisor::start(&catalog);
        assert_eq!(
            supervisor.ensure_lsp_for_language(&catalog, "rust"),
            vec!["rust".to_string()]
        );
        assert!(supervisor
            .ensure_lsp_for_language(&catalog, "rust")
            .is_empty());
        assert_eq!(supervisor.running_names(), vec!["rust".to_string()]);
        supervisor.shutdown();
    }

    #[test]
    fn process_specs_parse_all_runtime_fields() {
        let root = tempfile::tempdir().unwrap();
        let specs = load_process_specs(
            "test",
            root.path(),
            &json!({
                "rust": {
                    "command": "rust-analyzer",
                    "args": ["--stdio"],
                    "env": {"RUST_LOG": "warn"},
                    "languages": ["rust"],
                    "events": ["turn_finished"],
                    "autoStart": true
                }
            }),
        )
        .unwrap();
        assert_eq!(specs[0].args, vec!["--stdio"]);
        assert_eq!(specs[0].languages, vec!["rust"]);
        assert!(specs[0].auto_start);
    }
}
