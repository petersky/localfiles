# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

**localfiles** — A Rust MCP server that indexes local files and provides keyword search over stdio. Built with `rmcp` (MCP SDK), `tantivy` (full-text search), and `notify` (file watching).

## Build & Run

```bash
cargo build              # Compile
cargo run                # Start MCP server on stdio (Ctrl+C to stop)
```

## Architecture

- `src/main.rs` — Entry point: stdio MCP server, background watcher task
- `src/server.rs` — MCP handler with 3 tools (search, index_paths, status)
- `src/indexer.rs` — Tantivy index: create, add/remove/search documents
- `src/watcher.rs` — File watcher bridge (notify -> tokio mpsc channel)

Shared state (`Arc<RwLock<SharedState>>`) coordinates the MCP handler, indexer, and background watcher task. The watcher debounces events for 500ms before re-indexing.

## MCP Tools

- **search** — Keyword query returning file paths, snippets, and relevance scores
- **index_paths** — Add files/directories to the index and watch list (recursive)
- **status** — Show number of indexed files, watched paths, index location

## Key Details

- All logging goes to stderr (stdout reserved for MCP stdio protocol)
- Index stored at `$TMPDIR/localfiles_index`
- Supports text file extensions only (.rs, .py, .js, .md, .txt, .toml, etc.)
- 10MB file size limit; binary files are skipped
