# openapi-lsp

A small Rust language server for `$ref` go-to-definition in OpenAPI YAML and JSON files.
It communicates over stdio and works with Neovim's built-in LSP client.

## Build

Install [rustup](https://rustup.rs/) and a C compiler (for the Tree-sitter grammars), then:

```sh
cargo build --release --locked
```

`rust-toolchain.toml` pins Rust 1.98.1, rustfmt, and Clippy. The project uses Rust 2024.
The executable is `target/release/openapi-lsp`. It takes no arguments; stdout is
reserved for LSP messages, and errors go to stderr.

## Neovim

Add this to your configuration for Neovim 0.11 or newer, changing the executable path:

```lua
vim.lsp.config("openapi_lsp", {
  cmd = { "/absolute/path/to/openapi-lsp/target/release/openapi-lsp" },
  filetypes = { "yaml", "json" },
  root_markers = { "openapi.yaml", "openapi.yml", "openapi.json", ".git" },
})
vim.lsp.enable("openapi_lsp")

vim.api.nvim_create_autocmd("LspAttach", {
  callback = function(event)
    local client = vim.lsp.get_client_by_id(event.data.client_id)
    if client and client.name == "openapi_lsp" then
      vim.keymap.set("n", "gd", vim.lsp.buf.definition, {
        buffer = event.buf,
        desc = "Go to reference definition",
      })
    end
  end,
})
```

Open `examples/openapi.yaml`, place the cursor inside either `$ref` value, and
press `gd`. Use `:checkhealth vim.lsp` if the server does not attach, and
`:lua vim.cmd.edit(vim.lsp.log.get_filename())` to inspect the client log.

This configuration attaches to YAML and JSON files, including schema fragments
without a top-level `openapi` field. Navigation is syntactic: a string-valued
`$ref` is recognized wherever it occurs, including examples.

## Supported behavior

- Same-file pointers such as `#/components/schemas/Pet`.
- Relative and absolute local file references, including mixed YAML/JSON targets.
- References to a whole document, array elements, escaped keys (`~0`, `~1`), and
  percent-encoded URI fragments and filenames.
- YAML block and flow mappings/sequences, plain and quoted scalar references.
- Unsaved source and target buffers, with UTF-16 LSP positions and Unicode text.
- Navigation through intact syntax while a document contains an unrelated syntax error.

Targets select the mapping key or array element. Resolution follows one reference
at a time, so recursive schemas do not cause recursive loading. Missing targets,
unreadable files, invalid references, and unsupported reference forms return no definition.

The initial version does not implement diagnostics, completion, hover, rename,
find-references, or OpenAPI schema validation. It does not fetch HTTP resources,
resolve JSON Schema anchors or `$dynamicRef`, expand YAML aliases/merge keys,
or support multi-document YAML streams or block-scalar references. References
inside mappings scoped by `$id` are skipped until schema resource resolution is
implemented. Duplicate mapping keys are treated as ambiguous.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

CI runs the same checks. Tests cover parsing and source positions, reference
resolution, and an in-memory LSP session including unsaved changes and shutdown.

The single crate has four modules:

| Module | Responsibility |
| --- | --- |
| `main.rs` | Start stdio and join transport threads |
| `server.rs` | LSP lifecycle, requests, and open document ownership |
| `document.rs` | Tree-sitter syntax navigation and source positions |
| `refs.rs` | File URI and JSON Pointer resolution |

The application handles messages sequentially. Documents use full text
synchronization and are reparsed on change. Closed target files are read on each
request, avoiding stale disk caches. Add incremental parsing or caching when
large specifications demonstrate a need.
