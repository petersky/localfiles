use std::path::{Path, PathBuf};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub enum FileEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Removed(PathBuf),
}

/// Create a new file watcher and a channel receiver for file events.
///
/// The caller keeps the `RecommendedWatcher` alive and uses it to register paths.
/// File events are sent through the returned mpsc receiver.
pub fn new_watcher() -> anyhow::Result<(RecommendedWatcher, mpsc::Receiver<FileEvent>)> {
    let (tx, rx) = mpsc::channel::<FileEvent>(256);

    let watcher =
        notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let events: Vec<FileEvent> = match event.kind {
                    EventKind::Create(_) => {
                        event.paths.into_iter().map(FileEvent::Created).collect()
                    }
                    EventKind::Modify(_) => {
                        event.paths.into_iter().map(FileEvent::Modified).collect()
                    }
                    EventKind::Remove(_) => {
                        event.paths.into_iter().map(FileEvent::Removed).collect()
                    }
                    _ => vec![],
                };
                for fe in events {
                    let _ = tx.blocking_send(fe);
                }
            }
        })?;

    Ok((watcher, rx))
}

/// Helper to add a path to a watcher.
pub fn watch_path(watcher: &mut RecommendedWatcher, path: &Path) -> anyhow::Result<()> {
    watcher.watch(path, RecursiveMode::Recursive)?;
    Ok(())
}
