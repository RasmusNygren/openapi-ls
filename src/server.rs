use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    GotoDefinitionParams, Hover, HoverContents, HoverParams, InitializeParams, InitializeResult,
    Location, MarkupContent, MarkupKind, OneOf, PositionEncodingKind, Range, ReferenceParams,
    ServerCapabilities, ServerInfo, TextDocumentPositionParams, TextDocumentSyncCapability,
    TextDocumentSyncKind,
    notification::{
        DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Exit, Notification as _,
    },
    request::{GotoDefinition, HoverRequest, References, Request as _, Shutdown},
};
use url::Url;

use crate::{
    Result,
    document::{Document, Format},
    refs,
};

struct OpenDocument {
    version: i32,
    document: Document,
}

#[derive(Default)]
struct Server {
    documents: HashMap<Url, OpenDocument>,
    roots: Vec<PathBuf>,
    hover_markdown: bool,
}

pub(crate) fn run(connection: Connection) -> Result<()> {
    let (id, params) = connection.initialize_start()?;
    let params: InitializeParams = serde_json::from_value(params)?;
    let hover_markdown = params
        .capabilities
        .text_document
        .as_ref()
        .and_then(|text| text.hover.as_ref())
        .and_then(|hover| hover.content_format.as_ref())
        .and_then(|formats| formats.first())
        == Some(&MarkupKind::Markdown);
    let mut roots: Vec<_> = params
        .workspace_folders
        .unwrap_or_default()
        .iter()
        .filter_map(|folder| Url::parse(folder.uri.as_str()).ok()?.to_file_path().ok())
        .collect();
    #[allow(deprecated)] // Neovim and other single-workspace clients still send rootUri.
    if roots.is_empty()
        && let Some(uri) = params.root_uri
        && let Ok(path) = Url::parse(uri.as_str())?.to_file_path()
    {
        roots.push(path);
    }
    let result = InitializeResult {
        capabilities: ServerCapabilities {
            position_encoding: Some(PositionEncodingKind::UTF16),
            text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
            definition_provider: Some(OneOf::Left(true)),
            references_provider: Some(OneOf::Left(true)),
            hover_provider: Some(true.into()),
            ..Default::default()
        },
        server_info: Some(ServerInfo {
            name: env!("CARGO_PKG_NAME").into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        }),
    };
    connection.initialize_finish(id, serde_json::to_value(result)?)?;

    let mut server = Server {
        roots,
        hover_markdown,
        ..Server::default()
    };
    let mut shutdown = false;
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                let response = if shutdown {
                    Response::new_err(
                        request.id,
                        ErrorCode::InvalidRequest as i32,
                        "server has shut down".into(),
                    )
                } else if request.method == Shutdown::METHOD {
                    shutdown = true;
                    Response::new_ok(request.id, ())
                } else {
                    server.request(request)
                };
                connection.sender.send(response.into())?;
            }
            Message::Notification(notification) if notification.method == Exit::METHOD => {
                return if shutdown {
                    Ok(())
                } else {
                    Err("exit received before shutdown".into())
                };
            }
            Message::Notification(notification) if !shutdown => {
                if let Err(error) = server.notification(notification) {
                    eprintln!("openapi-lsp: {error}");
                }
            }
            _ => {}
        }
    }
    Err("client disconnected without exiting".into())
}

impl Server {
    fn request(&self, request: Request) -> Response {
        let result = match request.method.as_str() {
            GotoDefinition::METHOD => {
                serde_json::from_value::<GotoDefinitionParams>(request.params).map(|params| {
                    self.target_at(params.text_document_position_params, false)
                        .and_then(|target| {
                            Ok(serde_json::to_value(target.map(|(location, _)| location))?)
                        })
                })
            }
            HoverRequest::METHOD => {
                serde_json::from_value::<HoverParams>(request.params).map(|params| {
                    self.hover(params)
                        .and_then(|hover| Ok(serde_json::to_value(hover)?))
                })
            }
            References::METHOD => {
                serde_json::from_value::<ReferenceParams>(request.params).map(|params| {
                    self.references(params)
                        .and_then(|references| Ok(serde_json::to_value(references)?))
                })
            }
            _ => {
                return Response::new_err(
                    request.id,
                    ErrorCode::MethodNotFound as i32,
                    format!("unsupported method: {}", request.method),
                );
            }
        };
        match result {
            Err(error) => Response::new_err(
                request.id,
                ErrorCode::InvalidParams as i32,
                error.to_string(),
            ),
            Ok(Ok(value)) => Response::new_ok(request.id, value),
            Ok(Err(error)) => {
                // A missing/unreadable referenced file must not take down the editor session.
                eprintln!("openapi-lsp: {} failed: {error}", request.method);
                Response::new_ok(request.id, serde_json::Value::Null)
            }
        }
    }

    fn notification(&mut self, notification: Notification) -> Result<()> {
        match notification.method.as_str() {
            DidOpenTextDocument::METHOD => {
                let params: DidOpenTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                let item = params.text_document;
                let uri = Url::parse(item.uri.as_str())?;
                let format = match item.language_id.as_str() {
                    "json" => Format::Json,
                    "yaml" => Format::Yaml,
                    _ => Format::for_path(uri.path()),
                };
                self.documents.insert(
                    uri,
                    OpenDocument {
                        version: item.version,
                        document: Document::new(item.text, format)?,
                    },
                );
            }
            DidChangeTextDocument::METHOD => {
                let params: DidChangeTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                let uri = Url::parse(params.text_document.uri.as_str())?;
                if let Some(open) = self.documents.get_mut(&uri) {
                    if params.text_document.version <= open.version {
                        return Ok(());
                    }
                    if params
                        .content_changes
                        .iter()
                        .any(|change| change.range.is_some())
                    {
                        return Err("expected full document changes".into());
                    }
                    if let Some(change) = params.content_changes.into_iter().last() {
                        open.document = Document::new(change.text, open.document.format)?;
                        open.version = params.text_document.version;
                    }
                }
            }
            DidCloseTextDocument::METHOD => {
                let params: DidCloseTextDocumentParams =
                    serde_json::from_value(notification.params)?;
                self.documents
                    .remove(&Url::parse(params.text_document.uri.as_str())?);
            }
            _ => {}
        }
        Ok(())
    }

    fn document(&self, uri: &Url) -> Result<Cow<'_, Document>> {
        if let Some(open) = self.documents.get(uri) {
            return Ok(Cow::Borrowed(&open.document));
        }
        let path = uri.to_file_path().map_err(|()| "invalid local file URI")?;
        let text = std::fs::read_to_string(&path)?;
        // ponytail: read closed files on demand; add caching and invalidation if I/O becomes slow.
        Ok(Cow::Owned(Document::new(
            text,
            Format::for_path(uri.path()),
        )?))
    }

    fn target_at(
        &self,
        position: TextDocumentPositionParams,
        allow_symbol: bool,
    ) -> Result<Option<(Location, Range)>> {
        let uri = Url::parse(position.text_document.uri.as_str())?;
        let Some(source) = self.documents.get(&uri) else {
            return Ok(None);
        };
        let Some(reference) = source.document.reference_at(position.position) else {
            return Ok(allow_symbol
                .then(|| source.document.symbol_at(position.position))
                .flatten()
                .map(|range| {
                    (
                        Location {
                            uri: position.text_document.uri,
                            range,
                        },
                        range,
                    )
                }));
        };
        let Some(target) = refs::resolve(&uri, &reference.value) else {
            return Ok(None);
        };
        let source_range = if position.position < reference.range.start
            || position.position >= reference.range.end
        {
            source
                .document
                .symbol_at(position.position)
                .unwrap_or(reference.range)
        } else {
            reference.range
        };
        let document = self.document(&target.document)?;
        let Some(range) = document.target(&target.pointer) else {
            return Ok(None);
        };
        Ok(Some((
            Location {
                uri: target.document.as_str().parse()?,
                range,
            },
            source_range,
        )))
    }

    fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let Some((location, range)) = self.target_at(params.text_document_position_params, true)?
        else {
            return Ok(None);
        };
        let document = self.document(&Url::parse(location.uri.as_str())?)?;
        let Some(preview) = document.preview(location.range) else {
            return Ok(None);
        };
        let contents = if self.hover_markdown {
            // A schema description can itself contain Markdown fences.
            let longest = preview
                .split(|ch| ch != '`')
                .map(str::len)
                .max()
                .unwrap_or(0);
            let fence = "`".repeat(3.max(longest + 1));
            let language = match document.format {
                Format::Json => "json",
                Format::Yaml => "yaml",
            };
            MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("{fence}{language}\n{preview}{fence}"),
            }
        } else {
            MarkupContent {
                kind: MarkupKind::PlainText,
                value: preview,
            }
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(contents),
            range: Some(range),
        }))
    }

    fn references(&self, params: ReferenceParams) -> Result<Vec<Location>> {
        let source_uri = Url::parse(params.text_document_position.text_document.uri.as_str())?;
        let Some((declaration, _)) = self.target_at(params.text_document_position, true)? else {
            return Ok(Vec::new());
        };
        let target_uri = Url::parse(declaration.uri.as_str())?;
        let target_document = self.document(&target_uri)?;
        let mut locations = Vec::new();
        let mut files = self.reference_files(&source_uri);
        files.insert(target_uri.clone());
        // ponytail: scan on each request; add a reverse index if large workspaces make this slow.
        for uri in files {
            let document = match self.document(&uri) {
                Ok(document) => document,
                Err(error) => {
                    eprintln!("openapi-lsp: cannot search {uri}: {error}");
                    continue;
                }
            };
            for reference in document.references() {
                if let Some(target) = refs::resolve(&uri, &reference.value)
                    && target.document == target_uri
                    && target_document.target(&target.pointer) == Some(declaration.range)
                {
                    locations.push(Location {
                        uri: uri.as_str().parse()?,
                        range: reference.range,
                    });
                }
            }
        }
        if params.context.include_declaration {
            locations.push(declaration);
        }
        locations.sort_by(|a, b| {
            (a.uri.as_str(), a.range.start, a.range.end).cmp(&(
                b.uri.as_str(),
                b.range.start,
                b.range.end,
            ))
        });
        locations.dedup();
        Ok(locations)
    }

    fn reference_files(&self, source: &Url) -> HashSet<Url> {
        let mut files: HashSet<_> = self.documents.keys().cloned().collect();
        let mut directories = self.roots.clone();
        if directories.is_empty()
            && let Ok(path) = source.to_file_path()
            && let Some(parent) = path.parent()
        {
            directories.push(parent.to_owned());
        }
        let mut visited = HashSet::new();
        while let Some(directory) = directories.pop() {
            if !visited.insert(directory.clone()) {
                continue;
            }
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    eprintln!(
                        "openapi-lsp: cannot search {}: {error}",
                        directory.display()
                    );
                    continue;
                }
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') || matches!(name.as_ref(), "target" | "node_modules") {
                    continue;
                }
                let path = entry.path();
                if kind.is_dir() {
                    directories.push(path);
                } else if kind.is_file()
                    && matches!(
                        path.extension().and_then(|extension| extension.to_str()),
                        Some("yaml" | "yml" | "json")
                    )
                    && let Ok(uri) = Url::from_file_path(path)
                {
                    files.insert(uri);
                }
            }
        }
        files
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use std::{thread, time::Duration};

    use super::*;

    fn response(client: &Connection) -> Response {
        match client
            .receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
        {
            Message::Response(response) => response,
            message => panic!("expected response, got {message:?}"),
        }
    }

    fn notify(client: &Connection, method: &str, params: Value) {
        client
            .sender
            .send(Notification::new(method.into(), params).into())
            .unwrap();
    }

    fn request(client: &Connection, method: &str, params: Value) -> Response {
        client
            .sender
            .send(Request::new(1.into(), method.into(), params).into())
            .unwrap();
        response(client)
    }

    #[test]
    fn lsp_session_tracks_edits_disk_files_and_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("models.json");
        std::fs::write(&path, "{\"Pet\": {}}\n").unwrap();
        let source = Url::from_file_path(directory.path().join("openapi.yaml")).unwrap();
        let target = Url::from_file_path(&path).unwrap();
        let (client, connection) = Connection::memory();
        let worker = thread::spawn(|| run(connection));

        let initialized = request(&client, "initialize", json!({"capabilities": {}}));
        let capabilities = &initialized.response_result.unwrap()["capabilities"];
        assert_eq!(capabilities["definitionProvider"], true);
        assert_eq!(capabilities["referencesProvider"], true);
        assert_eq!(capabilities["hoverProvider"], true);
        assert_eq!(capabilities["positionEncoding"], "utf-16");
        assert_eq!(capabilities["textDocumentSync"], 1);
        notify(&client, "initialized", json!({}));
        notify(
            &client,
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": source.as_str(), "languageId": "yaml", "version": 1, "text": "$ref: 'models.json#/Pet'\n"
            }}),
        );
        let definition = json!({"textDocument": {"uri": source.as_str()}, "position": {"line": 0, "character": 10}});
        let result = request(&client, "textDocument/definition", definition.clone())
            .response_result
            .unwrap();
        assert_eq!(result["uri"], target.as_str());
        assert_eq!(result["range"]["start"], json!({"line": 0, "character": 1}));
        let hover = request(&client, "textDocument/hover", definition.clone())
            .response_result
            .unwrap();
        assert_eq!(hover["contents"]["kind"], "plaintext");
        assert!(
            hover["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("\"Pet\": {}")
        );
        let mut references = definition.clone();
        references["context"] = json!({"includeDeclaration": false});
        assert_eq!(
            request(&client, "textDocument/references", references)
                .response_result
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );

        notify(
            &client,
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": target.as_str(), "languageId": "json", "version": 1, "text": "{\n  \"Pet\": {}\n}\n"
            }}),
        );
        assert_eq!(
            request(&client, "textDocument/definition", definition.clone())
                .response_result
                .unwrap()["range"]["start"]["line"],
            1
        );
        notify(
            &client,
            "textDocument/didChange",
            json!({"textDocument": {"uri": target.as_str(), "version": 2}, "contentChanges": [{"text": "{}"}]}),
        );
        assert!(
            request(&client, "textDocument/definition", definition.clone())
                .response_result
                .unwrap()
                .is_null()
        );
        notify(
            &client,
            "textDocument/didChange",
            json!({"textDocument": {"uri": target.as_str(), "version": 1}, "contentChanges": [{"text": "{\"Pet\": {}}"}]}),
        );
        assert!(
            request(&client, "textDocument/definition", definition.clone())
                .response_result
                .unwrap()
                .is_null()
        );
        notify(
            &client,
            "textDocument/didClose",
            json!({"textDocument": {"uri": target.as_str()}}),
        );
        assert_eq!(
            request(&client, "textDocument/definition", definition.clone())
                .response_result
                .unwrap()["range"]["start"]["line"],
            0
        );

        notify(
            &client,
            "textDocument/didChange",
            json!({"textDocument": {"uri": source.as_str(), "version": 2}, "contentChanges": [{"text": "$ref: '#/Pet'\nPet: {}\n"}]}),
        );
        let result = request(&client, "textDocument/definition", definition.clone())
            .response_result
            .unwrap();
        assert_eq!(result["uri"], source.as_str());
        assert_eq!(result["range"]["start"]["line"], 1);
        notify(
            &client,
            "textDocument/didChange",
            json!({"textDocument": {"uri": source.as_str(), "version": 3}, "contentChanges": [{"text": "$ref: 'missing.yaml#/Pet'\n"}]}),
        );
        assert!(
            request(&client, "textDocument/definition", definition.clone())
                .response_result
                .unwrap()
                .is_null()
        );
        notify(
            &client,
            "textDocument/didOpen",
            json!({"bad": "notification"}),
        );
        assert_eq!(
            request(&client, "textDocument/definition", json!({}))
                .response_result
                .unwrap_err()
                .code,
            ErrorCode::InvalidParams as i32
        );
        assert_eq!(
            request(&client, "unknown/method", Value::Null)
                .response_result
                .unwrap_err()
                .code,
            ErrorCode::MethodNotFound as i32
        );
        assert!(
            request(&client, "shutdown", Value::Null)
                .response_result
                .unwrap()
                .is_null()
        );
        assert_eq!(
            request(&client, "textDocument/definition", definition)
                .response_result
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest as i32
        );
        notify(&client, "exit", Value::Null);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn references_and_hover_include_closed_files_and_unsaved_edits() {
        let directory = tempfile::tempdir().unwrap();
        let root = Url::from_directory_path(directory.path()).unwrap();
        let uri = root.join("models.json").unwrap();
        let source = root.join("specs/open.yaml").unwrap();
        let closed = root.join("closed.yml").unwrap();
        std::fs::create_dir(directory.path().join("specs")).unwrap();
        std::fs::create_dir(directory.path().join("target")).unwrap();
        std::fs::write(
            directory.path().join("models.json"),
            r#"{"Pét/name":{"description":"on disk"},"Other":{}}"#,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("closed.yml"),
            "$ref: './models.json#/P%C3%A9t~1name'\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("specs/open.yaml"),
            "$ref: '../models.json#/Other'\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("target/ignored.yaml"),
            "$ref: '../models.json#/P%C3%A9t~1name'\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("scoped.yaml"),
            "$id: elsewhere.json\n$ref: './models.json#/P%C3%A9t~1name'\n",
        )
        .unwrap();
        let (client, connection) = Connection::memory();
        let worker = thread::spawn(|| run(connection));
        request(
            &client,
            "initialize",
            json!({
                "workspaceFolders": [{"uri": root.as_str(), "name": "test"}],
                "capabilities": {"textDocument": {"hover": {"contentFormat": ["markdown", "plaintext"]}}}
            }),
        );
        notify(&client, "initialized", json!({}));
        notify(
            &client,
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": source.as_str(), "languageId": "yaml", "version": 1,
                "text": "$ref: '../models.json#/P%C3%A9t~1name'\n"
            }}),
        );
        let usage = json!({"textDocument": {"uri": source.as_str()}, "position": {"line": 0, "character": 12}});
        let mut references = usage.clone();
        references["context"] = json!({"includeDeclaration": false});
        let found = request(&client, "textDocument/references", references.clone())
            .response_result
            .unwrap();
        let found = found.as_array().unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0]["uri"], closed.as_str());
        assert_eq!(found[1]["uri"], source.as_str());
        assert_eq!(
            found[1]["range"]["start"],
            json!({"line": 0, "character": 6})
        );
        let hover = request(&client, "textDocument/hover", usage.clone())
            .response_result
            .unwrap();
        assert_eq!(hover["contents"]["kind"], "markdown");
        assert!(
            hover["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("on disk")
        );
        assert_eq!(hover["range"], found[1]["range"]);

        notify(
            &client,
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri.as_str(), "languageId": "json", "version": 1,
                "text": "{\n  \"Pét/name\": {\"description\": \"unsaved ``` fence\"},\n  \"Other\": {}\n}\n"
            }}),
        );
        let declaration =
            json!({"textDocument": {"uri": uri.as_str()}, "position": {"line": 1, "character": 5}});
        references["textDocument"] = declaration["textDocument"].clone();
        references["position"] = declaration["position"].clone();
        references["context"]["includeDeclaration"] = json!(true);
        let found = request(&client, "textDocument/references", references.clone())
            .response_result
            .unwrap();
        assert_eq!(found.as_array().unwrap().len(), 3);
        assert!(
            found
                .as_array()
                .unwrap()
                .iter()
                .any(|location| location["uri"] == uri.as_str()
                    && location["range"]["start"]["line"] == 1)
        );
        for position in [usage.clone(), declaration] {
            let hover = request(&client, "textDocument/hover", position)
                .response_result
                .unwrap();
            let text = hover["contents"]["value"].as_str().unwrap();
            assert!(text.starts_with("````json\n"), "{text}");
            assert!(text.contains("unsaved ``` fence"));
            assert!(text.ends_with("````"));
        }
        notify(
            &client,
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": source.as_str(), "version": 2}, "contentChanges": [{"text": "{}"}]
            }),
        );
        assert_eq!(
            request(&client, "textDocument/references", references.clone())
                .response_result
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        notify(
            &client,
            "textDocument/didClose",
            json!({"textDocument": {"uri": source.as_str()}}),
        );
        assert_eq!(
            request(&client, "textDocument/references", references)
                .response_result
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        for method in ["textDocument/hover", "textDocument/references"] {
            assert_eq!(
                request(&client, method, json!({}))
                    .response_result
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidParams as i32
            );
        }
        request(&client, "shutdown", Value::Null);
        notify(&client, "exit", Value::Null);
        worker.join().unwrap().unwrap();
    }
}
