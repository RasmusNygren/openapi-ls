# openapi-ls

A small Rust language server for `$ref` navigation, references, and hover in OpenAPI YAML and JSON files.
It communicates over stdio and works with Neovim's built-in LSP client.

Prebuilt binaries will be published on [GitHub Releases](https://github.com/RasmusNygren/openapi-ls/releases).
Once installed on your `PATH`, use `cmd = { "openapi-ls" }` in the Neovim
configuration below. Building from source is only needed for development.

## Build

Install [rustup](https://rustup.rs/) and a C compiler (for the Tree-sitter grammars), then:

```sh
cargo build --release --locked
```

`rust-toolchain.toml` pins Rust 1.98.1, rustfmt, and Clippy. The project uses Rust 2024.
The executable is `target/release/openapi-ls`. It takes no arguments; stdout is
reserved for LSP messages, and errors go to stderr.

## Neovim

Add this to your configuration for Neovim 0.11 or newer, changing the executable path:

```lua
vim.lsp.config("openapi_ls", {
  cmd = { "/absolute/path/to/openapi-ls/target/release/openapi-ls" },
  filetypes = { "yaml", "json" },
  root_markers = { "openapi.yaml", "openapi.yml", "openapi.json", ".git" },
})
vim.lsp.enable("openapi_ls")

vim.api.nvim_create_autocmd("LspAttach", {
  callback = function(event)
    local client = vim.lsp.get_client_by_id(event.data.client_id)
    if client and client.name == "openapi_ls" then
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

## Releases

Releases use [cargo-dist](https://axodotdev.github.io/cargo-dist/) 0.33.0 and
GitHub Actions. Manually running the Release workflow builds binaries for:

- macOS: Apple Silicon and Intel.
- Linux: ARM64 and x86-64 (glibc).
- Windows: x86-64.

GitHub Releases receive the archives, checksums, and shell/PowerShell installers.
Users of these binaries do not need Rust or a C compiler. The release workflow
runs the existing formatting, Clippy, and test checks before publishing. Pull
requests validate the release plan without publishing anything. Crates.io
publishing remains disabled.

To publish a release:

1. Update the version in `Cargo.toml`, run `cargo check` to update `Cargo.lock`,
   and commit the changes. The first release can use the existing `0.1.0` version.
2. Push the changes to GitHub. The workflow must be present on the default branch
   for GitHub to display its manual trigger.
3. Open **Actions → Release → Run workflow**, select the branch to release, and
   enter a matching version tag such as `v0.1.0` in the `tag` input.
4. Click **Run workflow**. The workflow builds the selected branch and creates
   the tag when it publishes the GitHub Release; you do not need to push a tag.

The default `tag` value, `dry-run`, builds and uploads workflow artifacts without
publishing a release. Pushing tags does not trigger the release workflow.

The workflow uses GitHub's automatic `GITHUB_TOKEN`; no custom release secret is
needed. Wait for the Release workflow to finish before sharing the release.

To maintain or validate the workflow locally:

```sh
cargo install cargo-dist --version 0.33.0 --locked
dist generate
dist generate --check
dist plan
dist build
```

Edit `dist-workspace.toml` and regenerate `.github/workflows/release.yml` with
`dist generate`; do not edit the generated workflow directly. `dist build`
packages the current platform locally without publishing. CI supplies the native
build tools for the C code in Tree-sitter and the grammars.
