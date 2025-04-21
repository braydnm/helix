use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    io,
    os::fd::{AsRawFd, RawFd},
    path::{Path, PathBuf},
};

// Linux (inotify) specific imports
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use tokio::io::{Interest, unix::AsyncFd};
use helix_view::handlers::{AutoReloadEvent, Handlers, FileEventKind, FileEvent};

use crate::handlers::watcher::Watcher;

#[derive(Debug)]
pub struct InotifyWatcher {
    inotify: Inotify,
    paths: HashMap<WatchDescriptor, PathBuf>,
    inverse_paths: HashMap<PathBuf, WatchDescriptor>,
    polling_paths: HashSet<PathBuf>,
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // ... (implementation remains the same as before)
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags < 0 { return Err(io::Error::last_os_error()); }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl InotifyWatcher {
    /// Creates a new instance of the inotify-based file watcher.
    pub fn new() -> io::Result<Self> {
        let inotify = Inotify::init()?; // Initializes inotify and gets FD
        let fd = inotify.as_raw_fd();

        // Ensure FD is non-blocking
        set_nonblocking(fd)?;

        Ok(Self {
            inotify, // Takes ownership
            inverse_paths: HashMap::new(),
            paths: HashMap::new(),
            polling_paths: HashSet::new(),
        })
    }
    async fn process_event(
        &mut self,
        event: inotify::Event<&OsStr>,
    ) -> io::Result<Option<FileEvent>> {
        assert!(!event.mask.contains(EventMask::ISDIR));

        let path = self.paths.get(&event.wd);

        if path.is_none() {
            log::warn!(
                "Failed to process event for {:?}, this might be a phantom event for a file already remove",
                path
            );
            return Ok(None);
        }

        let path = path.unwrap().clone();

        let kind = if event.mask.contains(EventMask::DELETE_SELF)
            || event.mask.contains(EventMask::MOVED_FROM)
            || event.mask.contains(EventMask::MOVE_SELF)
            || event.mask.contains(EventMask::DELETE)
        {
            log::trace!("Detected delete for {:?}, event = {:?}", path, event);
            self.handle_delete(path.clone());
            FileEventKind::Delete
        } else {
            // TODO: This is pretty racey
            log::trace!("Detected change in {:?}, event = {:?}", path, event);
            let metadata = tokio::fs::metadata(&path).await.unwrap();
            FileEventKind::Modify(metadata.modified().unwrap())
        };

        Ok(Some(FileEvent { path, kind }))
    }
}

impl super::Watcher for InotifyWatcher {
    fn polling_paths(&mut self) -> &mut HashSet<PathBuf> {
        &mut self.polling_paths
    }

    fn wait_fd(&self) -> AsyncFd<RawFd> {
        AsyncFd::with_interest(self.inotify.as_raw_fd(), Interest::READABLE).unwrap()
    }

    /// Adds a path to watch for file-related events.
    /// Accepts both files and directories. If a directory is watched,
    /// events for files created/modified/deleted within it will be reported.
    fn inner_watch<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        // Define the events we care about for files
        let watch_mask = WatchMask::MODIFY | WatchMask::DELETE_SELF | WatchMask::MOVE_SELF;

        let wd = match self.inotify.watches().add(path, watch_mask) {
            Ok(wd) => {
                log::trace!("Started watching {:?} with wd {:?}", path, wd);
                wd
            }
            Err(e) => {
                log::error!("Failed to watch {:?} due to {:?}", path, e);
                return Err(e);
            }
        };
        self.paths.insert(wd.clone(), path.into());
        self.inverse_paths.insert(path.into(), wd);

        Ok(())
    }

    /// Removes a path from being watched.
    fn inner_unwatch<P: AsRef<Path>>(&mut self, path: &P) -> io::Result<()> {
        // rm_watch can return Err if wd is invalid (e.g., already removed by DELETE_SELF)
        let wd = self.inverse_paths.remove_entry(path.as_ref()).unwrap();
        self.paths.remove(&wd.1).unwrap();
        match self.inotify.watches().remove(wd.1.clone()) {
            Ok(_) => Ok(()),

            Err(e)
                if e.kind() == io::ErrorKind::InvalidInput
                    || e.kind() == io::ErrorKind::NotFound =>
            {
                log::debug!(
                    "Failed to remove watch (path = {:?}, wd = {:?}) with {:?}, may have already been removed",
                    path.as_ref(),
                    wd.1,
                    e
                );
                // Watch might have already been removed (e.g., DELETE_SELF)
                Ok(()) // Treat as success in this case
            }
            Err(e) => {
                log::error!(
                    "Failed to remove watch (path = {:?}, wd = {:?}) with {:?}",
                    path.as_ref(), wd.1, e
                );
                Err(e)
            }
        }
    }

    fn commit(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn try_read_platform_event(&mut self) -> io::Result<Option<FileEvent>> {
        let mut buf = [0; 1024];
        match self.inotify.read_events(&mut buf) {
            Ok(events) => {
            log::trace!("Read events {:?}", events);
                for event in events {
                    if let Some(file_event) = self.process_event(event).await? {
                        return Ok(Some(file_event)); // Return the first relevant event
                    }
                }
                Ok(None) // No relevant events in this batch
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                log::debug!("Ignoring would block read");
                Ok(None)
            }
            Err(e) => {
                log::error!("Failed to read inotify events {:?}", e);
                Err(e)
            }
        }
    }
}
