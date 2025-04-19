use std::{
    collections::HashSet,
    io,
    os::fd::RawFd,
    path::PathBuf,
    sync::Arc,
    time::SystemTime,
};

use helix_view::handlers::{FileEvent, FileEventKind};
use tokio::io::{unix::AsyncFd, Interest};
use tokio::sync::{mpsc, RwLock};
use watchman_client::{prelude::*, SubscriptionData};

#[derive(Debug)]
pub struct WatchmanWatcher {
    event_rx: mpsc::UnboundedReceiver<FileEvent>,
    watched_paths: Arc<RwLock<HashSet<PathBuf>>>,
    signal_pipe: (RawFd, RawFd),
    _subscription_handle: tokio::task::JoinHandle<()>,
}

fn create_signal_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    for &fd in &fds {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags < 0 {
                libc::close(fds[0]);
                libc::close(fds[1]);
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                libc::close(fds[0]);
                libc::close(fds[1]);
                return Err(io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                libc::close(fds[0]);
                libc::close(fds[1]);
                return Err(io::Error::last_os_error());
            }
        }
    }

    Ok((fds[0], fds[1]))
}

fn signal_pipe(write_fd: RawFd) -> io::Result<()> {
    let byte: u8 = 1;
    let ret = unsafe {
        libc::write(
            write_fd,
            &byte as *const u8 as *const libc::c_void,
            1,
        )
    };
    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        Err(err)
    } else {
        Ok(())
    }
}

impl WatchmanWatcher {
    pub fn new() -> io::Result<Self> {
        let signal_pipe = create_signal_pipe()?;
        let write_fd = signal_pipe.1;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let watched_paths = Arc::new(RwLock::new(HashSet::new()));
        let watched_paths_clone = watched_paths.clone();

        let subscription_handle = tokio::spawn(async move {
            if let Err(e) = Self::run_subscription(event_tx, watched_paths_clone, write_fd).await {
                log::error!("Watchman subscription task failed: {:?}", e);
            }
        });

        Ok(Self {
            event_rx,
            watched_paths,
            signal_pipe,
            _subscription_handle: subscription_handle,
        })
    }

    async fn run_subscription(
        event_tx: mpsc::UnboundedSender<FileEvent>,
        watched_paths: Arc<RwLock<HashSet<PathBuf>>>,
        signal_write_fd: RawFd,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let client = Connector::new().connect().await?;

        let current_dir = std::env::current_dir()?;
        let canonical_path = CanonicalPath::canonicalize(&current_dir)?;
        let resolved = client.resolve_root(canonical_path).await?;

        let (mut subscription, _response) = client
            .subscribe::<NameOnly>(
                &resolved,
                SubscribeRequest {
                    expression: Some(Expr::FileType(FileType::Regular)),
                    ..Default::default()
                },
            )
            .await?;

        loop {
            match subscription.next().await {
                Ok(data) => {
                    match data {
                        SubscriptionData::FilesChanged(result) => {
                            if let Some(files) = result.files {
                                let watched = watched_paths.read().await;

                                for file_info in files {
                                    let file_path = file_info.name.into_inner();
                                    let full_path = if file_path.is_absolute() {
                                        file_path
                                    } else {
                                        resolved.project_root().join(&file_path)
                                    };

                                    if watched.contains(&full_path) {
                                        let metadata = tokio::fs::metadata(&full_path).await;
                                        let kind = match metadata {
                                            Ok(meta) => {
                                                if let Ok(modified) = meta.modified() {
                                                    FileEventKind::Modify(modified)
                                                } else {
                                                    FileEventKind::Modify(SystemTime::now())
                                                }
                                            }
                                            Err(_) => FileEventKind::Delete,
                                        };

                                        let event = FileEvent {
                                            path: full_path,
                                            kind,
                                        };

                                        if event_tx.send(event).is_err() {
                                            log::debug!("Event receiver closed, exiting subscription");
                                            return Ok(());
                                        }

                                        let _ = signal_pipe(signal_write_fd);
                                    }
                                }
                            }
                        }
                        SubscriptionData::Canceled => {
                            log::warn!("Watchman subscription canceled");
                            break;
                        }
                        SubscriptionData::StateEnter { state_name, .. } => {
                            log::trace!("Watchman state enter: {}", state_name);
                        }
                        SubscriptionData::StateLeave { state_name, .. } => {
                            log::trace!("Watchman state leave: {}", state_name);
                        }
                    }
                }
                Err(e) => {
                    log::error!("Error reading from watchman subscription: {:?}", e);
                    break;
                }
            }
        }

        Ok(())
    }

    pub async fn watch(&mut self, path: PathBuf) -> io::Result<()> {
        log::trace!("Watching file {:?} (watchman)", path);
        let watched_paths = self.watched_paths.clone();
        tokio::spawn(async move {
            let mut paths = watched_paths.write().await;
            paths.insert(path);
        });
        Ok(())
    }

    pub async fn unwatch(&mut self, path: PathBuf) -> io::Result<()> {
        log::trace!("Unwatching {:?} (watchman)", path);
        let watched_paths = self.watched_paths.clone();
        tokio::spawn(async move {
            let mut paths = watched_paths.write().await;
            paths.remove(&path);
        });
        Ok(())
    }

    pub fn wait_fd(&self) -> AsyncFd<RawFd> {
        AsyncFd::with_interest(self.signal_pipe.0, Interest::READABLE)
            .expect("Failed to create AsyncFd for signal pipe")
    }

    pub async fn try_read_platform_event(&mut self) -> io::Result<Option<FileEvent>> {
        let mut buf = [0u8; 1];
        let _ = unsafe {
            libc::read(
                self.signal_pipe.0,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };

        match self.event_rx.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Watchman subscription disconnected",
                ))
            }
        }
    }

    pub fn commit(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub async fn poll_created_files(&mut self) -> io::Result<Vec<FileEvent>> {
        Ok(Vec::new())
    }
}

impl Drop for WatchmanWatcher {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.signal_pipe.0);
            libc::close(self.signal_pipe.1);
        }
    }
}

pub fn spawn_config_watcher(
    auto_reload_tx: tokio::sync::mpsc::Sender<helix_view::handlers::AutoReloadEvent>,
) {
    tokio::spawn(async move {
        if let Err(e) = run_config_watcher(auto_reload_tx).await {
            log::error!("Config watcher failed: {:?}", e);
        }
    });
}

async fn run_config_watcher(
    auto_reload_tx: tokio::sync::mpsc::Sender<helix_view::handlers::AutoReloadEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = Connector::new().connect().await?;

    let config_dir = helix_loader::config_dir();
    if !config_dir.exists() {
        log::debug!("Config directory does not exist, skipping config watcher");
        return Ok(());
    }

    let canonical_path = CanonicalPath::canonicalize(&config_dir)?;
    let resolved = client.resolve_root(canonical_path).await?;

    let (mut subscription, _response) = client
        .subscribe::<NameOnly>(
            &resolved,
            SubscribeRequest {
                expression: Some(Expr::Suffix(vec!["toml".into()])),
                ..Default::default()
            },
        )
        .await?;

    log::info!("Watchman config watcher started for {:?}", config_dir);

    loop {
        match subscription.next().await {
            Ok(data) => {
                match data {
                    SubscriptionData::FilesChanged(result) => {
                        if result.is_fresh_instance {
                            continue;
                        }

                        if let Some(files) = result.files {
                            for file_info in files {
                                let file_name = file_info.name.into_inner();
                                let file_name_str = file_name.to_string_lossy();

                                if file_name_str.ends_with("config.toml")
                                    || file_name_str.ends_with("languages.toml")
                                    || file_name_str.contains("themes/")
                                {
                                    log::info!("Config file changed: {:?}", file_name);
                                    let _ = auto_reload_tx.send(
                                        helix_view::handlers::AutoReloadEvent::ConfigChanged
                                    ).await;
                                    break;
                                }
                            }
                        }
                    }
                    SubscriptionData::Canceled => {
                        log::warn!("Config watcher subscription canceled");
                        break;
                    }
                    _ => {}
                }
            }
            Err(e) => {
                log::error!("Config watcher error: {:?}", e);
                break;
            }
        }
    }

    Ok(())
}
