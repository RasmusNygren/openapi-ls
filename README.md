# openapi-lsp

A small Rust language server for `$ref` navigation, references, and hover in OpenAPI YAML and JSON files.
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
      vim.keymap.set("n", "grr", vim.lsp.buf.references, {
        buffer = event.buf,
        desc = "Find references",
      })
      vim.keymap.set("n", "K", vim.lsp.buf.hover, {
        buffer = event.buf,
        desc = "Preview definition",
      })
    end
  end,
})
```

Open `examples/openapi.yaml`, place the cursor inside either `$ref` value, and
press `gd` to jump, `K` to preview the target, or `grr` to find its references.
Hover and find-references also work on a definition key such as `Pet`.
You can invoke these directly with `:lua vim.lsp.buf.hover()` and
`:lua vim.lsp.buf.references()` if you use different key mappings.
After rebuilding the executable, restart Neovim to load the new server.
Use `:checkhealth vim.lsp` if the server does not attach, and
`:lua vim.cmd.edit(vim.lsp.log.get_filename())` to inspect the client log.

This configuration attaches to YAML and JSON files, including schema fragments
without a top-level `openapi` field. Navigation is syntactic: a string-valued
`$ref` is recognized wherever it occurs, including examples.

`root_markers` is optional. It sets the workspace boundary for find-references.
With a workspace root, references are searched recursively in its YAML/JSON files
and all open buffers. Without a root, the current file's directory is searched
recursively, together with open buffers. Searches include unopened files and use
unsaved buffers in preference to disk. Hidden entries, `target`, `node_modules`,
and symlinks are skipped during directory scanning; `.gitignore` is not interpreted.
Workspace roots are read at initialization; restart the server after changing them.

## Supported behavior

- Same-file pointers such as `#/components/schemas/Pet`.
- Relative and absolute local file references, including mixed YAML/JSON targets.
- References to a whole document, array elements, escaped keys (`~0`, `~1`), and
  percent-encoded URI fragments and filenames.
- YAML block and flow mappings/sequences, plain and quoted scalar references.
- Unsaved source and target buffers, with UTF-16 LSP positions and Unicode text.
- Navigation through intact syntax while a document contains an unrelated syntax error.
- Find-references from a `$ref` or declaration, including unopened workspace files.
- Hover previews of the target's YAML/JSON, limited to 40 lines/about 4 KB plus a
  truncation marker. Clients without Markdown support receive plain text.

Targets select the mapping key or array element. Resolution follows one reference
at a time, so recursive schemas do not cause recursive loading. Missing targets,
unreadable files, invalid references, and unsupported reference forms return no definition.

The server does not implement diagnostics, completion, rename, or OpenAPI schema
validation. It does not fetch HTTP resources,
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
resolution, hover previews, and in-memory LSP sessions including workspace
references, unsaved changes, and shutdown.

The single crate has four modules:

| Module | Responsibility |
| --- | --- |
| `main.rs` | Start stdio and join transport threads |
| `server.rs` | LSP lifecycle, requests, and open document ownership |
| `document.rs` | Tree-sitter syntax navigation and source positions |
| `refs.rs` | File URI and JSON Pointer resolution |

The application handles messages sequentially. Documents use full text
synchronization and are reparsed on change. Closed target files are read on each
request, avoiding stale disk caches. Find-references scans on demand without a
persistent index. Add incremental parsing, caching, or a reverse reference index
when large specifications demonstrate a need.
