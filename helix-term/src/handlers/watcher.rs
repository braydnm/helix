#[cfg(not(feature = "no-auto-reload"))]
pub mod watchman;

#[cfg(not(feature = "no-auto-reload"))]
pub use watchman::WatchmanWatcher;

#[cfg(not(feature = "no-auto-reload"))]
pub use watchman::spawn_config_watcher;
