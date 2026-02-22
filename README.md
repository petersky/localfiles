# localfiles

A Rust MCP (Model Context Protocol) server that indexes local files and provides full-text keyword search over stdio. Files are watched for changes and automatically re-indexed.

## Stack

- [rmcp](https://crates.io/crates/rmcp) — Official Rust MCP SDK (stdio transport)
- [tantivy](https://github.com/quickwit-oss/tantivy) — Embedded full-text search engine
- [notify](https://github.com/notify-rs/notify) — Cross-platform file watcher
- [tokio](https://tokio.rs) — Async runtime

## Build & Run

```bash
cargo build
cargo run        # Starts MCP server on stdio (Ctrl+C to stop)
```

## MCP Tools

### `index_paths`

Add files or directories to the search index. Directories are indexed recursively. Paths are watched for changes and automatically re-indexed.

**Parameters:**
- `paths` (array of strings) — File or directory paths to index

### `search`

Search indexed files by keyword. Returns matching file paths, text snippets, and relevance scores.

**Parameters:**
- `query` (string) — Keyword query
- `limit` (number, optional) — Max results to return (default: 10)

### `status`

Show current index status: number of indexed files, watched paths, and index storage location.

**No parameters.**

## Architecture

```
┌──────────────┐     stdio      ┌───────────────┐
│  MCP Client  │ ◄────────────► │  MCP Server   │
└──────────────┘                │  (server.rs)  │
                                └───────┬───────┘
                                        │
                          Arc<RwLock<SharedState>>
                                        │
                         ┌──────────────┼──────────────┐
                         │              │              │
                   ┌─────▼─────┐   ┌────▼────┐   ┌─────▼─────┐
                   │  Indexer  │   │ Watcher │   │ Background│
                   │ (tantivy) │   │ (notify)│   │   Task    │
                   └───────────┘   └─────────┘   │(debounced)│
                                                 └───────────┘
```

- **`src/main.rs`** — Entry point: stdio MCP server, spawns background watcher task
- **`src/server.rs`** — MCP handler with 3 tools (`search`, `index_paths`, `status`)
- **`src/indexer.rs`** — Tantivy index: create, add/remove/search documents
- **`src/watcher.rs`** — File watcher bridge (notify → tokio mpsc channel)

Shared state is held behind `Arc<RwLock<>>`. MCP tools acquire read locks for search/status and write locks for index_paths. The background watcher task debounces file events for 500ms before re-indexing in batch.

## Configuration

### Claude Code

Add to your MCP settings (`.claude/settings.json` or project-level):

```json
{
  "mcpServers": {
    "localfiles": {
      "command": "cargo",
      "args": ["run", "--manifest-path", "/path/to/localfiles/Cargo.toml"]
    }
  }
}
```

Or, after building with `cargo build --release`:

```json
{
  "mcpServers": {
    "localfiles": {
      "command": "/path/to/localfiles/target/release/localfiles"
    }
  }
}
```

## Testing

```bash
cargo test               # Run all 37 unit tests
cargo bench              # Run criterion benchmarks (full)
cargo bench -- --test    # Quick check (compile + single iteration)
```

Unit tests cover the core `indexer.rs` module: index creation/migration, file type detection, indexing, search (keyword and field-based filters), file removal, listing, reading, status, snippet extraction, and line matching. Each test uses an isolated temporary directory.

Benchmarks use a synthetic dataset (1000 files across 8 extensions and 10 subdirectories) to measure indexing, commit, and search performance. Results are written to `target/criterion/` with HTML reports.

## Details

- **Index storage:** `$TMPDIR/localfiles_index` (persists across restarts)
- **Supported file types:** `.rs`, `.py`, `.js`, `.ts`, `.jsx`, `.tsx`, `.json`, `.toml`, `.yaml`, `.yml`, `.html`, `.css`, `.scss`, `.sh`, `.c`, `.cpp`, `.h`, `.hpp`, `.java`, `.go`, `.rb`, `.php`, `.sql`, `.xml`, `.csv`, `.md`, `.txt`, `.log`, `.cfg`, `.conf`, `.ini`, `.env`, plus `Makefile` and `Dockerfile`
- **File size limit:** 10MB
- **Binary files:** Skipped (non-UTF-8 files are ignored)
- **Logging:** All tracing output goes to stderr (stdout is reserved for the MCP stdio protocol)
