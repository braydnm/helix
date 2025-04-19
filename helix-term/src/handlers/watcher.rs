use std::{
    collections::HashSet,
    io,
    os::fd::RawFd,
    path::{Path, PathBuf},
    time::SystemTime,
};

use helix_view::handlers::{FileEventKind, FileEvent};
use tokio::io::unix::AsyncFd;

pub trait Watcher {
    fn commit(&mut self) -> io::Result<()>;

    fn inner_watch<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()>;
    fn inner_unwatch<P: AsRef<Path>>(&mut self, path: &P) -> io::Result<()>;

    fn polling_paths(&mut self) -> &mut HashSet<PathBuf>;

    fn wait_fd(&self) -> AsyncFd<RawFd>;
    async fn try_read_platform_event(&mut self) -> io::Result<Option<FileEvent>>;

    async fn poll_created_files(&mut self) -> io::Result<Vec<FileEvent>> {
        let mut ret = Vec::new();
        for path in self.polling_paths().iter() {
            match tokio::fs::metadata(path).await {
                Ok(m) => {
                    log::trace!("Detected file creation {:?}", path);
                    ret.push(FileEvent {
                        path: path.clone(),
                        kind: FileEventKind::Modify(m.modified()?),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => log::error!(
                    "Unexpected failure stat-ing non-existant watched file {:?}",
                    path
                ),
            }
        }

        for event in &ret {
            self.handle_create(&event.path);
        }

        Ok(ret)
    }

    fn handle_delete(&mut self, path: PathBuf) -> io::Result<()> {
        let _ = self.inner_unwatch(&path);
        self.polling_paths().insert(path);
        Ok(())
    }

    fn handle_create(&mut self, path: &PathBuf) {
        let path = self.polling_paths().take(path).unwrap();
        let _ = self.inner_watch(path);
    }

    async fn watch(&mut self, path: PathBuf) -> io::Result<()> {
        let exists = tokio::fs::try_exists(&path).await;

        log::trace!("Watching file {:?}", path);
        match exists {
            Ok(true) => {
                log::trace!("File exists, watching");
                self.inner_watch(path)?;
            }
            _ => {
                log::trace!("File does not exist, falling back to polling");
                self.polling_paths().insert(path);
            }
        }

        Ok(())
    }

    async fn unwatch(&mut self, path: PathBuf) -> io::Result<()> {
        log::trace!("Unwatching {:?}", path);
        if !self.polling_paths().remove(&path) {
            log::trace!("File actually existed");
            self.inner_unwatch(&path)?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub mod inotify;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub mod kqueue;

#[cfg(target_os = "linux")]
use crate::handlers::watcher::inotify::InotifyWatcher;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use crate::handlers::watcher::kqueue::KqueueWatcher;

#[cfg(target_os = "linux")]
pub type RecommendedWatcher = InotifyWatcher;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
pub type RecommendedWatcher = KqueueWatcher;
