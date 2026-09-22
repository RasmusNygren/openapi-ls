# openapi-ls

A small language server for OpenAPI YAML and JSON. Supports go-to-definition,
find-references, and hover previews.

## Installation

Download a binary from [GitHub Releases](https://github.com/RasmusNygren/openapi-ls/releases)
and place it on your `PATH`, or install from a cloned checkout:

```sh
cargo install --path . --locked
```

Building from source requires [rustup](https://rustup.rs/) and a C compiler for Tree-sitter.

## Neovim

Add this to your configuration (Neovim 0.11+):

```lua
vim.lsp.config("openapi_ls", {
  cmd = { "openapi-ls" },
  filetypes = { "yaml", "json" },
})
vim.lsp.enable("openapi_ls")
```

References are searched recursively in the current file's directory and all open
buffers. Set `root_markers` to search a whole workspace instead. Use
`:checkhealth vim.lsp` if the server does not attach.

Currently supports local files and JSON Pointer references. Remote URLs, JSON Schema
`$id`/anchors, YAML aliases, and schema validation are not supported.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

## Releases

Update the version in `Cargo.toml` and `Cargo.lock`, commit and push, then run
**Actions → Release → Run workflow** with a matching tag such as `v0.1.0`.
Use `dry-run` to test packaging without publishing. Tag pushes do not publish releases.

Release configuration lives in [dist-workspace.toml](dist-workspace.toml).
After changing it, regenerate the workflow with `dist generate` using cargo-dist 0.33.0.
