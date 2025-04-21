use std::collections::HashSet;
use std::io;
use std::os::fd::RawFd;
use std::path::Path;
use std::path::PathBuf;

use helix_view::handlers::FileEvent;
use helix_view::handlers::FileEventKind;
use kqueue::EventData;
use kqueue::EventFilter;
use kqueue::FilterFlag;
use kqueue::Ident;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use std::os::fd::AsRawFd;

use super::Watcher;

#[derive(Debug)]
pub struct KqueueWatcher {
    kqueue: kqueue::Watcher,
    polling_paths: HashSet<PathBuf>,
}

impl KqueueWatcher {
    /// Creates a new instance of the kqueue-based file watcher.
    pub fn new() -> io::Result<Self> {
        let kqueue = kqueue::Watcher::new().unwrap();
        Ok(Self {
            kqueue,
            polling_paths: HashSet::new(),
        })
    }
}

impl Watcher for KqueueWatcher {
    fn commit(&mut self) -> io::Result<()> {
        match self.kqueue.watch() {
            Ok(_) => {
                log::trace!("Commited changes to watcher");
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to commit to watcher {:?}", e);
                Err(e)
            }
        }
    }

    fn wait_fd(&self) -> AsyncFd<RawFd> {
        AsyncFd::with_interest(self.kqueue.as_raw_fd(), Interest::READABLE).unwrap()
    }

    fn polling_paths(&mut self) -> &mut HashSet<PathBuf> {
        &mut self.polling_paths
    }

    /// Adds a path to watch for file-related events.
    /// Accepts both files and directories. If a directory is watched,
    /// events for files created/modified/deleted within it may trigger
    /// events on the directory's FD.
    fn inner_watch<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let event_filter = EventFilter::EVFILT_VNODE;
        let filter_flags = FilterFlag::NOTE_DELETE
            | FilterFlag::NOTE_WRITE
            | FilterFlag::NOTE_EXTEND
            | FilterFlag::NOTE_ATTRIB
            | FilterFlag::NOTE_LINK
            | FilterFlag::NOTE_RENAME
            | FilterFlag::NOTE_REVOKE;

        match self
            .kqueue
            .add_filename(path.as_ref(), event_filter, filter_flags)
        {
            Ok(_) => log::trace!("Successfully added watch for {:?}", path.as_ref()),
            Err(e) => log::error!(
                "Failed to add watch to file {:?} due to {:?}",
                path.as_ref(),
                e
            ),
        };

        Ok(())
    }

    /// Removes a path from being watched.
    fn inner_unwatch<P: AsRef<Path>>(&mut self, path: &P) -> io::Result<()> {
        log::trace!("Removing watch for {:?}", path.as_ref());
        match self.kqueue.remove_filename(path, EventFilter::EVFILT_VNODE) {
            Ok(_) => log::trace!("Removed watch on {:?}", path.as_ref()),
            Err(e) => log::error!(
                "Failed to remove watch on {:?} due to {:?}",
                path.as_ref(),
                e
            ),
        }
        Ok(())
    }

    /// Asynchronously waits for file events and returns the next one found.
    async fn try_read_platform_event(&mut self) -> io::Result<Option<FileEvent>> {
        let event = self.kqueue.poll(None);
        if event.is_none() {
            log::trace!("Got no events from kqueue");
            return Ok(None);
        }

        let event = event.unwrap();

        match event {
            kqueue::Event {
                data: EventData::Vnode(data),
                ident: Ident::Filename(_, filename),
            } => {
                let path: PathBuf = filename.into();
                let kind = match data {
                    kqueue::Vnode::Delete | kqueue::Vnode::Revoke | kqueue::Vnode::Rename => {
                        log::trace!("Detected delete on file {:?}", path);
                        let _ = self.handle_delete(path.clone());
                        FileEventKind::Delete
                    }
                    _ => {
                        // TODO: This is racy?
                        log::trace!("Detected modify on file {:?}", path);
                        let metadata = tokio::fs::metadata(&path).await.unwrap();
                        FileEventKind::Modify(metadata.modified()?)
                    }
                };

                Ok(Some(FileEvent { path, kind }))
            }
            _ => unreachable!(),
        }
    }
}
