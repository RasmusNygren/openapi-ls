use std::collections::HashMap;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    GotoDefinitionParams, InitializeParams, InitializeResult, Location, OneOf,
    PositionEncodingKind, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind,
    notification::{
        DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Exit, Notification as _,
    },
    request::{GotoDefinition, Request as _, Shutdown},
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
}

pub(crate) fn run(connection: Connection) -> Result<()> {
    let (id, params) = connection.initialize_start()?;
    let _: InitializeParams = serde_json::from_value(params)?;
    let result = InitializeResult {
        capabilities: ServerCapabilities {
            position_encoding: Some(PositionEncodingKind::UTF16),
            text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
            definition_provider: Some(OneOf::Left(true)),
            ..Default::default()
        },
        server_info: Some(ServerInfo {
            name: env!("CARGO_PKG_NAME").into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        }),
    };
    connection.initialize_finish(id, serde_json::to_value(result)?)?;

    let mut server = Server::default();
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
        if request.method != GotoDefinition::METHOD {
            return Response::new_err(
                request.id,
                ErrorCode::MethodNotFound as i32,
                format!("unsupported method: {}", request.method),
            );
        }
        let params = match serde_json::from_value::<GotoDefinitionParams>(request.params) {
            Ok(params) => params,
            Err(error) => {
                return Response::new_err(
                    request.id,
                    ErrorCode::InvalidParams as i32,
                    error.to_string(),
                );
            }
        };
        match self.definition(params) {
            Ok(location) => Response::new_ok(request.id, location),
            Err(error) => {
                // A missing/unreadable referenced file must not take down the editor session.
                eprintln!("openapi-lsp: definition failed: {error}");
                Response::new_ok(request.id, Option::<Location>::None)
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

    fn definition(&self, params: GotoDefinitionParams) -> Result<Option<Location>> {
        let position = params.text_document_position_params;
        let uri = Url::parse(position.text_document.uri.as_str())?;
        let Some(source) = self.documents.get(&uri) else {
            return Ok(None);
        };
        let Some(reference) = source.document.reference_at(position.position) else {
            return Ok(None);
        };
        let Some(target) = refs::resolve(&uri, &reference) else {
            return Ok(None);
        };

        let disk_document;
        let document = if let Some(open) = self.documents.get(&target.document) {
            &open.document
        } else {
            let path = target
                .document
                .to_file_path()
                .map_err(|()| "invalid local file URI")?;
            let text = std::fs::read_to_string(&path)?;
            // ponytail: read closed files on demand; add caching and invalidation if I/O becomes slow.
            disk_document = Document::new(text, Format::for_path(target.document.path()))?;
            &disk_document
        };
        let Some(range) = document.target(&target.pointer) else {
            return Ok(None);
        };
        Ok(Some(Location {
            uri: target.document.as_str().parse()?,
            range,
        }))
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
}
