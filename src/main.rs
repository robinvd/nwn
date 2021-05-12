use std::{
    collections::HashMap,
    error::Error,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use futures::{future, Stream, StreamExt, TryStreamExt};
use regex::Regex;
use ropey::{Rope, RopeSlice};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::UnixListener,
};
use tower_lsp::{
    jsonrpc::Result as RpcResult, lsp_types::*, Client, LanguageServer, LspService, Server,
};

fn time() -> usize {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as usize
}

#[derive(Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
struct Frame {
    file: String,
    line: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawEntry {
    frames: Vec<Frame>,
    #[serde(flatten)]
    inner: RawEntryInner,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum RawEntryInner {
    Simple {
        out: String,
        // insert: Option<String>,
        kind: Option<RawEntryKind>,
    },
    Edits {
        changes: Vec<TextEdit>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RawEntryKind {
    Output,
    Debug,
    Error,
    Regex,
}

impl RawEntry {
    fn from_json(reader: impl Read) -> Result<Self, Box<dyn std::error::Error>> {
        let entry = serde_json::de::from_reader(reader);
        match entry {
            Ok(entry) => Ok(entry),
            Err(err) => {
                log::error!("loading err {:?}", err);
                Err(Box::new(err))
            }
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Runner {
    command: String,
    args: Vec<String>,
}

impl Runner {
    async fn run_collect(&self, path: &str, input: ropey::Rope) -> io::Result<Vec<RawEntry>> {
        self.run(path, input).await?.try_collect().await
    }

    async fn run(
        &self,
        path: &str,
        input: ropey::Rope,
    ) -> io::Result<impl Stream<Item = io::Result<RawEntry>>> {
        log::info!("run2");

        let mut dir = std::env::temp_dir();
        dir.push(&format!(
            "nwn_conn{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));

        log::info!("path {:?}", dir);
        let mut listener = UnixListener::bind(&dir)?;

        log::info!("opened");
        let mut command = tokio::process::Command::new(&self.command);
        let mut handle = command
            .env("NWN_FILE_PATH", path)
            .env("NWN_CONNECTION_FD", &dir)
            .args(&self.args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        log::error!("spawned runtime:{}", time());

        let stdout = handle.stdout.take().unwrap();
        let stderr = handle.stderr.take().unwrap();

        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr).lines();

            while let Some(line) = reader.next().await {
                log::error!("inject msg: {:?}", line);
            }
        });
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stdout).lines();

            while let Some(line) = reader.next().await {
                log::warn!("inject msg: {:?}", line);
            }
        });

        tokio::spawn(async move {
            handle
                .await
                .map_err(|err| log::error!("err: {:?}", err))
                .ok();

            log::error!("runtime finished:{}", time())
        });

        let (right, _) = listener.accept().await?;
        let (read, mut write) = tokio::io::split(right);

        tokio::spawn(async move {
            for part in input.chunks() {
                let result = write.write_all(part.as_bytes()).await;
                if result.is_err() {
                    break;
                }
            }

            write.write_all(&[0]).await.ok();

            log::error!("finished contents write:{}", time());
        });

        log::info!("start loop");

        Ok(tokio::io::BufReader::new(read)
            .lines()
            .inspect(|line| log::info!("line: {:?}", line))
            .try_filter(|line| future::ready(!line.trim().is_empty()))
            .try_filter_map(|line| async move {
                log::info!("got line: {:?}", line);
                let entry = RawEntry::from_json(line.as_bytes());
                match entry {
                    Ok(entry) => Ok(Some(entry)),
                    Err(err) => {
                        log::error!("reading line json err: {:?}", err);
                        Ok(None)
                    }
                }
            }))
    }
}

#[derive(Debug, Default)]
struct RunData {
    entries: HashMap<Frame, RunEntry>,
    hovers: HashMap<Frame, RunEntry>,
    changes: Vec<TextEdit>,
}

#[derive(Debug)]
struct RunEntry {
    insert_type: Option<String>,
    out: Vec<Arc<String>>,
}

impl RunData {
    pub async fn from_raw(mut raws: impl Stream<Item = RawEntry> + Unpin, path: &str) -> Self {
        let mut new_entries = HashMap::new();
        let mut new_hovers = HashMap::new();
        let mut changes = Vec::new();
        while let Some(entry) = raws.next().await {
            // for entry in raws {
            match entry.inner {
                RawEntryInner::Simple { out, kind } => {
                    let text = Arc::new(out);
                    let kind = kind.unwrap_or(RawEntryKind::Output);

                    match kind {
                        RawEntryKind::Output => {
                            if let Some(frame) =
                                entry.frames.into_iter().find(|frame| frame.file == path)
                            {
                                let run_entry = new_entries.entry(frame).or_insert(RunEntry {
                                    out: Vec::new(),
                                    insert_type: None,
                                });
                                run_entry.out.push(text.clone())
                            }
                        }
                        RawEntryKind::Debug => {
                            if let Some(frame) = entry
                                .frames
                                .into_iter()
                                .rev()
                                .find(|frame| frame.file == path)
                            {
                                let run_entry = new_entries.entry(frame).or_insert(RunEntry {
                                    out: Vec::new(),
                                    insert_type: None,
                                });
                                run_entry.out.push(text.clone())
                            }
                        }
                        RawEntryKind::Error => {
                            for frame in entry.frames.into_iter() {
                                let run_entry = new_hovers.entry(frame).or_insert(RunEntry {
                                    out: Vec::new(),
                                    insert_type: None,
                                });
                                run_entry.out.push(text.clone())
                            }
                        }
                        RawEntryKind::Regex => {
                            if let Some(frame) =
                                entry.frames.into_iter().find(|frame| frame.file == path)
                            {
                                let run_entry = new_entries.entry(frame).or_insert(RunEntry {
                                    out: Vec::new(),
                                    insert_type: None,
                                });
                                run_entry.out.push(text.clone())
                            }
                        }
                    }
                }
                RawEntryInner::Edits {
                    changes: new_changes,
                } => changes.extend(new_changes),
            }
        }

        RunData {
            entries: new_entries,
            hovers: new_hovers,
            changes,
        }
    }
}

#[derive(Debug)]
struct Backend(Arc<BackendData>);

#[derive(Debug, Deserialize)]
struct LanguageConfig {
    extension: String,
    runner: Runner,
    line_comment: String,
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

#[derive(Debug)]
struct BackendData {
    client: Client,
    data: ArcSwap<RunData>,
    runner: Option<Runner>,
    config: ArcSwap<Config>,
    buffers: DashMap<Url, ropey::Rope>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self(Arc::new(BackendData {
            client,
            data: ArcSwap::default(),
            runner: None,
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

    pub async fn handle_save(&self, uri: &Url, contents: Option<Rope>) {
        log::info!("handle save");
        let config = self.language_config_for_uri(uri);
        if let Some(config) = config {
            self.reload_data(config, uri).await;

            // update comments
            if let Some(contents) = contents {
                self.update_comments(uri, &contents).await;
                self.update_diagnostics(uri, &contents).await;
            } else {
                log::warn!("no text in handle_save");
            }
        }
    }

    pub async fn reload_data(&self, config: Arc<LanguageConfig>, uri: &Url) {
        log::info!("start reload");
        let data = self.0.clone();
        let path = uri.path().to_owned();
        let buffer = self
            .0
            .buffers
            .get(uri)
            .map(|buffer| ropey::Rope::clone(&*buffer))
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

    fn starts_with(slice: &RopeSlice, text: &str) -> bool {
        if slice.len_chars() < text.chars().count() {
            return false;
        }
        slice.chars().zip(text.chars()).all(|(x, y)| x == y)
    }

    fn find_output_ranges<'a>(
        &self,
        uri: &Url,
        contents: &'a Rope,
    ) -> impl Iterator<Item = usize> + 'a {
        // TODO language idenpendant
        let output_start = "#>";
        contents
            .lines()
            .enumerate()
            .filter(move |(_, line)| Self::starts_with(line, output_start))
            .map(|(line, _)| line)
    }

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
                let mut text = format!("{}> ", config.line_comment);
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
                        format!("{}> {}\n", config.line_comment, escaped.trim_matches('"'))
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

        fn make_insert(
            contents: &Rope,
            config: &LanguageConfig,
            frame: &Frame,
            entry: &RunEntry,
        ) -> Option<TextEdit> {
            let insert_type = entry.insert_type.as_ref().unwrap();
            let line_text = contents.line(frame.line as usize - 1).to_string();

            config
                .places
                .iter()
                .filter(|place| &place.name == insert_type)
                .filter_map(|place| {
                    log::info!("trying: {:?}", place.regex);
                    let captures = place.regex.captures(&line_text)?;
                    let match_ = captures.get(place.insert_place)?;
                    log::info!("regex match: {:?}", match_);
                    let range = Range::new(
                        Position::new(frame.line - 1, match_.start() as u64),
                        Position::new(frame.line - 1, match_.end() as u64),
                    );
                    Some(TextEdit::new(range, entry.out.first()?.to_string()))
                })
                .next()
        }

        let config = self.language_config_for_uri(uri);
        if let Some(config) = config {
            let data = self.0.data.load();
            let mut changes: Vec<TextEdit> = data
                .entries
                .iter()
                .filter(|(frame, _)| frame.file == uri.path())
                .filter_map(|(frame, entry)| {
                    if entry.insert_type.is_some() {
                        make_insert(contents, &config, frame, &entry)
                    } else {
                        make_output_comment(contents, &config, frame, &entry)
                    }
                })
                .collect();

            changes.extend(data.changes.iter().cloned());

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
        self.0
            .buffers
            .insert(params.text_document.uri.clone(), buffer.clone());

        self.handle_save(&params.text_document.uri, Some(buffer))
            .await
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.0.buffers.remove(&params.text_document.uri);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        log::error!("change start:{}", time());
        let buffer: Rope = params.content_changes[0].text.as_str().into();
        self.0
            .buffers
            .insert(params.text_document.uri.clone(), buffer.clone());
        log::info!(
            "new buffer: {:?}",
            self.0
                .buffers
                .get(&params.text_document.uri)
                .map(|rope| rope.len_bytes())
        );

        let config = self.language_config_for_uri(&params.text_document.uri);
        if config.map(|config| config.on_change).unwrap_or_default() {
            self.handle_save(&params.text_document.uri, Some(buffer))
                .await
        }
        log::error!("change end:{}", time());
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let buffer = self
            .0
            .buffers
            .get(&params.text_document.uri)
            .as_deref()
            .cloned();
        self.handle_save(&params.text_document.uri, buffer).await
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
            for line_number in self
                .find_output_ranges(&params.text_document.uri, &buffer)
                .map(|n| n as u64)
            {
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
