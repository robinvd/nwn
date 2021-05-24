use std::{
    collections::HashMap,
    error::Error,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use futures::{lock::Mutex, StreamExt};
use regex::Regex;
use ropey::{Rope, RopeSlice};
use runner::RunData;
use serde::Deserialize;
use tower_lsp::{
    jsonrpc::Result as RpcResult, lsp_types::*, Client, LanguageServer, LspService, Server,
};
use util::rope_starts_with;

use crate::{protocol::Frame, runner::RunEntry};
use crate::{runner::Runner, util::time};

mod protocol;
mod runner;
mod util;

/// Config for a single programming language
#[derive(Debug, Deserialize)]
struct LanguageConfig {
    /// The file extension this language uses (ex .rs)
    extension: String,
    /// The details for how to run this language.
    runner: Runner,
    /// The prefix this language uses for line comments (ex "//")
    line_comment: String,
    /// If this language should be run on each change in the buffer, or only on save. TODO
    #[serde(default)]
    on_change: bool,

    #[serde(default)]
    places: Vec<Place>,
}

#[derive(Debug, Deserialize, Clone)]
struct Place {
    name: String,
    #[serde(with = "serde_regex")]
    regex: Regex,
    insert_place: usize,
}

/// Config for this app
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
struct Config {
    #[serde(deserialize_with = "Config::map_from_list")]
    languages: HashMap<String, Arc<LanguageConfig>>,
    #[serde(skip)]
    base_dir: PathBuf,
}

impl Config {
    fn map_from_list<'de, D>(d: D) -> Result<HashMap<String, Arc<LanguageConfig>>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let vec = <Vec<Arc<LanguageConfig>>>::deserialize(d)?;

        Ok(vec
            .into_iter()
            .map(|config| (config.extension.clone(), config))
            .collect())
    }

    pub fn from_default_config() -> Result<Self, Box<dyn Error + Send + Sync>> {
        let locations = &["./.nwn.json", "/home/robin.local/.config/nwn/.nwn.json"];

        locations
            .iter()
            .find_map(|path| {
                Self::from_filepath(Path::new(path))
                    .map_err(|err| log::warn!("config load err: {}", err))
                    .ok()
            })
            .ok_or_else(|| "no configs found".to_owned().into())
    }

    pub fn from_filepath(path: &Path) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mut config: Config = serde_json::from_reader(file).map_err(|e| e.to_string())?;
        config.base_dir = path.parent().unwrap().to_owned();
        for lang_config in config.languages.values_mut() {
            log::info!("replacing config values {:?}", lang_config);
            let runner = &mut Arc::get_mut(lang_config).unwrap().runner;
            let config_dir = config.base_dir.to_str().unwrap();
            runner.command = runner.command.replace("{runtimes}", config_dir);
            for arg in &mut runner.args {
                *arg = arg.replace("{runtimes}", config_dir);
            }
            log::info!("new config values {:?}", lang_config);
        }
        Ok(config)
    }
}

#[derive(Debug, Default)]
struct BufferData {
    /// The actual text in the buffer
    buffer: ropey::Rope,
    /// A lock for when we are using the buffer, so that updates happen serially
    locked: Arc<Mutex<()>>,
}

/// All the data the backend has
///
/// All of this data can only be accessed behind a read only reference
/// (&BackendData) thus all fields that have to be mutated after the initial
/// setup need to have interior mutability.
#[derive(Debug)]
struct BackendData {
    /// The client for communicating with the editor
    client: Client,
    data: ArcSwap<RunData>,
    /// The config
    config: ArcSwap<Config>,
    /// A buffer for each file
    ///
    /// The editor can send the whole content on each request
    /// but also only the diff, for this reason we save the bufferdata
    ///
    /// Lsp uses an url for identifying a buffer, so that is what we use as
    /// index.
    buffers: DashMap<Url, BufferData>,
}

/// The actual backend implementation
///
/// We wrap BackendData in an Arc (reference counted pointer), so that we can
/// share this around without worrying about lifetimes
#[derive(Debug)]
struct Backend(Arc<BackendData>);

impl Backend {
    pub fn new(client: Client) -> Self {
        Self(Arc::new(BackendData {
            client,
            data: ArcSwap::default(),
            config: ArcSwap::new(Arc::new(Config::default())),
            buffers: DashMap::new(),
        }))
    }

    pub async fn load_config(&self) {
        let config = match Config::from_default_config() {
            Err(err) => {
                log::error!("error loading config: {}", err);
                self.0
                    .client
                    .log_message(
                        MessageType::Warning,
                        format!("could not find a a config: {}", err),
                    )
                    .await;

                Config::default()
            }
            Ok(config) => config,
        };

        log::info!("found config: {:?}", config);

        self.0.config.store(Arc::new(config));
    }

    /// Get the LanguageConfig for this path
    pub fn language_config_for_uri(&self, uri: &Url) -> Option<Arc<LanguageConfig>> {
        let path = Path::new(uri.path());
        let config = path
            .extension()
            .map(|ext| ext.to_string_lossy())
            .and_then(|ext| {
                let config = self.0.config.load();
                config.languages.get(ext.as_ref()).cloned()
            });

        log::info!("found {:?} for {:?}", config, uri);

        config
    }

    pub async fn handle_save(&self, uri: &Url, contents: Rope) {
        log::info!("handle save");
        let config = self.language_config_for_uri(uri);
        if let Some(config) = config {
            self.reload_data(config, uri).await;

            // update comments
            self.update_comments(uri, &contents).await;
            self.update_diagnostics(uri, &contents).await;
        }
    }

    /// Reload the rundata for this language and path
    pub async fn reload_data(&self, config: Arc<LanguageConfig>, uri: &Url) {
        log::info!("start reload");
        let data = self.0.clone();
        let path = uri.path().to_owned();
        let buffer = self
            .0
            .buffers
            .get(uri)
            .map(|buffer| ropey::Rope::clone(&buffer.buffer))
            .unwrap_or_default();

        let res = config.runner.run(&path, buffer).await;
        let raw_data = match res {
            Ok(data) => data.filter_map(|item| async {
                match item {
                    Ok(val) => Some(val),
                    Err(err) => {
                        log::warn!("could not read reload entry line; skipping: {:?}", err);
                        None
                    }
                }
            }),
            Err(err) => {
                log::warn!("could not reload data, {}", err);
                return;
            }
        };
        let new_data = RunData::from_raw(Box::pin(raw_data), &path).await;
        log::info!("new: {:?}", new_data);
        data.data.store(Arc::new(new_data));
    }

    /// Find all line indices that have NwoN output
    ///
    /// NwoN output is indicated by a linecomment postfixed by a '>' character.
    fn find_output_ranges<'a>(&self, contents: &'a Rope) -> impl Iterator<Item = usize> + 'a {
        // TODO language idenpendant
        let output_start = "#>";
        contents
            .lines()
            .enumerate()
            .filter(move |(_, line)| rope_starts_with(line, output_start))
            .map(|(line, _)| line)
    }

    /// Send the diagnostics to the editor
    async fn update_diagnostics(&self, uri: &Url, contents: &Rope) {
        let data = self.0.data.load();
        let diags: Vec<Diagnostic> = data
            .hovers
            .iter()
            .filter(|(frame, _)| frame.file == uri.path())
            .map(|(frame, entry)| Diagnostic {
                range: Range::new(
                    Position::new(frame.line - 1, 0),
                    Position::new(
                        frame.line - 1,
                        contents.line(frame.line as usize - 1).len_chars() as u64,
                    ),
                ),
                severity: None,
                code: None,
                source: Some("nwn".to_string()),
                message: entry.out.iter().map(|s| s.as_str()).collect(),
                related_information: None,
                tags: None,
            })
            .collect();

        let response = self
            .0
            .client
            .publish_diagnostics(uri.clone(), diags, None)
            .await;

        log::info!("diag response: {:?}", response);
    }

    /// Given the current buffer and the runtime output, compute the changes required and send those to the editor.
    async fn update_comments(&self, uri: &Url, contents: &Rope) {
        /// find lines with comment output, result is an exclusive range
        fn find_prev_output(
            buffer: &Rope,
            comment_str: &str,
            line: usize,
        ) -> Option<(usize, usize)> {
            let comment_start = format!("{}>", comment_str);
            let mut curr = line + 1;
            while curr < buffer.len_lines()
                && buffer
                    .line(curr)
                    .chars()
                    .skip_while(|c| [' ', '\t'].contains(c))
                    .zip(comment_start.chars())
                    .all(|(x, y)| x == y)
            {
                curr += 1;
            }

            if line + 1 == curr {
                None
            } else {
                Some((line + 1, curr))
            }
        }

        fn make_output_comment(
            contents: &Rope,
            config: &LanguageConfig,
            frame: &Frame,
            entry: &RunEntry,
        ) -> Option<TextEdit> {
            let offset_range =
                find_prev_output(contents, &config.line_comment, frame.line as usize - 1);
            let indent: String = contents
                .line(frame.line as usize - 1)
                .chars()
                .take_while(|c| c.is_ascii_whitespace())
                .collect();
            let range = offset_range
                .map(|(start_line, end_line)| {
                    Range::new(
                        Position::new(start_line as u64, 0),
                        Position::new(end_line as u64, 0),
                    )
                })
                .unwrap_or_else(|| {
                    Range::new(Position::new(frame.line, 0), Position::new(frame.line, 0))
                });

            let single_line = false;
            let text = if single_line {
                let mut text = format!("{}{}> ", indent, config.line_comment);
                text.extend(entry.out.iter().map(|msg| format!("{:?} | ", msg)));
                text.push('\n');
                text
            } else {
                entry
                    .out
                    .iter()
                    .map(|msg| msg.lines())
                    .flatten()
                    .map(|msg| {
                        let escaped = format!("{:?}", msg);
                        format!(
                            "{}{}> {}\n",
                            indent,
                            config.line_comment,
                            escaped.trim_matches('"')
                        )
                    })
                    .collect()
            };
            if contents
                .lines_at(range.start.line as usize)
                .take(range.end.line as usize - range.start.line as usize)
                .map(|slice| slice.chars())
                .flatten()
                .eq(text.chars())
            {
                return None;
            }

            Some(TextEdit::new(range, text))
        }

        let config = self.language_config_for_uri(uri);
        if let Some(config) = config {
            let data = self.0.data.load();

            let changes = {
                let uri_path = uri.path();
                let mut entries_vec: Vec<_> = data
                    .entries
                    .iter()
                    .filter(|(frame, _entry)| frame.file == uri_path)
                    .collect();
                entries_vec.sort_by_key(|(frame, _)| frame.line);

                let mut entries = entries_vec.into_iter().peekable();

                let mut line_n = 0;
                let mut changes = Vec::new();
                while line_n < contents.len_lines() {
                    log::warn!("{}; loop line:", line_n);
                    let mut current_line_has_output = false;
                    while let Some((frame, entry)) = entries.peek() {
                        let frame_line = frame.line as usize - 1;
                        if frame_line <= line_n {
                            log::warn!("{}; appying entry", frame_line);
                            if let Some(change) =
                                make_output_comment(contents, &config, frame, entry)
                            {
                                changes.push(change)
                            }
                            entries.next().unwrap();
                            current_line_has_output |= frame_line == line_n;
                            let prev_output =
                                find_prev_output(contents, &config.line_comment, line_n);
                            if let Some((_start_line, end_line)) = prev_output {
                                line_n = end_line;
                            }
                        } else {
                            break;
                        }
                    }
                    log::warn!("{}; line has var {}", line_n, current_line_has_output);
                    if !current_line_has_output {
                        let prev_output = find_prev_output(contents, &config.line_comment, line_n);
                        if let Some((start_line, end_line)) = prev_output {
                            log::warn!("{}; delete", line_n);
                            changes.push(TextEdit::new(
                                Range::new(
                                    Position::new(start_line as u64, 0),
                                    Position::new(end_line as u64, 0),
                                ),
                                String::new(),
                            ));

                            line_n = end_line;
                        }
                    }
                    line_n += 1;
                }

                changes.extend(entries.filter_map(|(frame, entry)| {
                    make_output_comment(contents, &config, frame, entry)
                }));
                changes
            };

            log::info!("edits: {:?}", changes);

            let mut workspace_changes = HashMap::new();
            workspace_changes.insert(uri.clone(), changes);
            let response = self
                .0
                .client
                .apply_edit(WorkspaceEdit::new(workspace_changes))
                .await;

            log::info!("edit response: {:?}", response);
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> RpcResult<InitializeResult> {
        log::info!("initialize");
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        change: Some(TextDocumentSyncKind::Full),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            server_info: None,
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        log::info!("initialized");
        self.0
            .client
            .log_message(MessageType::Info, "server initialized!")
            .await;

        self.load_config().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let buffer: Rope = params.text_document.text.as_str().into();

        let lock = {
            let mut entry = self
                .0
                .buffers
                .entry(params.text_document.uri.clone())
                .or_default();
            entry.buffer = buffer.clone();
            entry.locked.clone()
        };
        lock.lock().await;

        self.handle_save(&params.text_document.uri, buffer).await
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.0.buffers.remove(&params.text_document.uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        log::error!("change start:{}", time());
        let buffer: Rope = params.content_changes[0].text.as_str().into();
        let lock = {
            let mut entry = self
                .0
                .buffers
                .entry(params.text_document.uri.clone())
                .or_default();
            entry.buffer = buffer.clone();
            entry.locked.clone()
        };
        lock.lock().await;

        log::info!(
            "new buffer: {:?}",
            self.0
                .buffers
                .get(&params.text_document.uri)
                .map(|buffer| buffer.buffer.len_bytes())
        );

        let config = self.language_config_for_uri(&params.text_document.uri);
        if config.map(|config| config.on_change).unwrap_or_default() {
            self.handle_save(&params.text_document.uri, buffer).await
        }
        log::error!("change end:{}", time());
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        Ok(None)
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> RpcResult<Option<Vec<FoldingRange>>> {
        if let Some(buffer) = self.0.buffers.get(&params.text_document.uri) {
            let mut folds: Vec<FoldingRange> = Vec::new();
            for line_number in self.find_output_ranges(&buffer.buffer).map(|n| n as u64) {
                match folds.last_mut() {
                    Some(last) if last.end_line + 1 == line_number => {
                        last.end_line += 1;
                    }
                    _ => folds.push(FoldingRange {
                        start_line: line_number,
                        end_line: line_number,
                        kind: Some(FoldingRangeKind::Comment),
                        ..Default::default()
                    }),
                }
            }

            Ok(Some(folds))
        } else {
            Ok(None)
        }
    }
}

#[tokio::main]
async fn main() {
    simple_logging::log_to_file("/tmp/test.log", log::LevelFilter::Info).unwrap();
    log_panics::init();
    log::info!("start");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, messages) = LspService::new(Backend::new);

    Server::new(stdin, stdout)
        .interleave(messages)
        .serve(service)
        .await;
}
