use std::future::Future;
use std::sync::Arc;

use notify::RecommendedWatcher;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use tokio::sync::RwLock;

use crate::indexer::FileIndex;
use crate::watcher;

/// Shared state between MCP handler, background watcher task, and indexer.
pub struct SharedState {
    pub index: FileIndex,
    pub watcher: RecommendedWatcher,
}

impl std::fmt::Debug for SharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedState").finish_non_exhaustive()
    }
}

pub type AppState = Arc<RwLock<SharedState>>;

// -- Tool parameter types --

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    #[schemars(description = "The keyword query to search for in indexed files")]
    pub query: String,
    #[schemars(description = "Maximum number of results to return (default: 10)")]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct IndexPathsRequest {
    #[schemars(description = "List of file or directory paths to index and watch")]
    pub paths: Vec<String>,
}

// -- MCP Server --

#[derive(Debug, Clone)]
pub struct FileSearchServer {
    state: AppState,
    tool_router: ToolRouter<FileSearchServer>,
}

#[tool_router]
impl FileSearchServer {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Search indexed files by keyword. Returns matching file paths and text snippets with relevance scores."
    )]
    async fn search(&self, Parameters(req): Parameters<SearchRequest>) -> String {
        let limit = req.limit.unwrap_or(10);
        let state = self.state.read().await;
        match state.index.search(&req.query, limit) {
            Err(e) => format!("Search error: {}", e),
            Ok(results) if results.is_empty() => "No results found.".to_string(),
            Ok(results) => {
                let mut out = String::new();
                for (i, r) in results.iter().enumerate() {
                    out.push_str(&format!(
                        "{}. {} (score: {:.2})\n   Path: {}\n   Snippet: {}\n\n",
                        i + 1,
                        r.file_name,
                        r.score,
                        r.file_path,
                        r.snippet
                    ));
                }
                out
            }
        }
    }

    #[tool(
        description = "Add file or directory paths to the search index. Directories are indexed recursively. Files are watched for changes and automatically re-indexed."
    )]
    async fn index_paths(&self, Parameters(req): Parameters<IndexPathsRequest>) -> String {
        let mut state = self.state.write().await;
        let mut total_indexed = 0u64;
        let mut errors = Vec::new();

        for path_str in &req.paths {
            let path = std::path::Path::new(path_str);
            if !path.exists() {
                errors.push(format!("Path does not exist: {}", path_str));
                continue;
            }
            if path.is_dir() {
                match state.index.index_directory(path) {
                    Ok(count) => total_indexed += count,
                    Err(e) => errors.push(format!("Error indexing {}: {}", path_str, e)),
                }
            } else {
                match state.index.index_file(path) {
                    Ok(()) => total_indexed += 1,
                    Err(e) => errors.push(format!("Error indexing {}: {}", path_str, e)),
                }
            }
            // Register with file watcher
            if let Err(e) = watcher::watch_path(&mut state.watcher, path) {
                errors.push(format!("Error watching {}: {}", path_str, e));
            }
        }

        // Commit all changes at once
        if let Err(e) = state.index.commit() {
            errors.push(format!("Commit failed: {}", e));
        }

        let mut msg = format!("Indexed {} files.", total_indexed);
        if !errors.is_empty() {
            msg.push_str(&format!("\nErrors:\n{}", errors.join("\n")));
        }
        msg
    }

    #[tool(
        description = "Show current index status: number of indexed files, watched paths, and index location."
    )]
    async fn status(&self) -> String {
        let state = self.state.read().await;
        let status = state.index.status();
        format!(
            "Index Status:\n  Files indexed: {}\n  Watched paths: {}\n  Index location: {}",
            status.num_files,
            if status.watched_paths.is_empty() {
                "(none)".to_string()
            } else {
                status.watched_paths.join(", ")
            },
            status.index_path,
        )
    }
}

#[tool_handler]
impl ServerHandler for FileSearchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "A local file search server. Use 'index_paths' to add directories, \
                 then 'search' to find files by keyword. Use 'status' to check index state."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
