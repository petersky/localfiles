mod server;
use localfiles::indexer;
use localfiles::watcher;

use std::sync::Arc;
use tokio::sync::RwLock;

use rmcp::transport::stdio;
use rmcp::ServiceExt;

use server::{FileSearchServer, SharedState};
use watcher::FileEvent;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // All tracing to stderr — stdout is reserved for MCP stdio protocol
    tracing_subscriber::fmt()
        .with_env_filter("localfiles=info")
        .with_writer(std::io::stderr)
        .init();

    // Create the file index
    let index = indexer::FileIndex::new(None)?;

    // Create the file watcher
    let (watcher_handle, mut event_rx) = watcher::new_watcher()?;

    // Shared state for MCP handler + background task
    let state = Arc::new(RwLock::new(SharedState {
        index,
        watcher: watcher_handle,
    }));

    // Spawn background task: debounced file event processing
    let state_bg = state.clone();
    tokio::spawn(async move {
        let mut pending: Vec<FileEvent> = Vec::new();
        loop {
            // Wait for the first event
            let event = event_rx.recv().await;
            match event {
                None => break, // channel closed
                Some(e) => pending.push(e),
            }

            // Debounce: collect events for 500ms
            let deadline =
                tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
            loop {
                match tokio::time::timeout_at(deadline, event_rx.recv()).await {
                    Ok(Some(e)) => pending.push(e),
                    _ => break,
                }
            }

            // Process batch under a single write lock
            let mut s = state_bg.write().await;
            for event in pending.drain(..) {
                match event {
                    FileEvent::Created(p) | FileEvent::Modified(p) => {
                        if let Err(e) = s.index.index_file(&p) {
                            tracing::warn!("Failed to re-index {}: {}", p.display(), e);
                        }
                    }
                    FileEvent::Removed(p) => {
                        if let Err(e) = s.index.remove_file(&p) {
                            tracing::warn!(
                                "Failed to remove {} from index: {}",
                                p.display(),
                                e
                            );
                        }
                    }
                }
            }
            if let Err(e) = s.index.commit() {
                tracing::warn!("Failed to commit after watcher batch: {}", e);
            }
        }
    });

    // Start MCP server on stdio
    tracing::info!("localfiles MCP server starting on stdio");
    let server = FileSearchServer::new(state);
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("Failed to start MCP server: {}", e);
    })?;

    service.waiting().await?;

    Ok(())
}
