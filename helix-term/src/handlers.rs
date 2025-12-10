use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_event::AsyncHook;

use crate::config::Config;
use crate::events;
use crate::handlers::auto_save::AutoSaveHandler;
#[cfg(not(feature = "no-auto-reload"))]
use crate::handlers::auto_reload::AutoReloadHandler;
use crate::handlers::signature_help::SignatureHelpHandler;

pub use helix_view::handlers::Handlers;

use self::document_colors::DocumentColorsHandler;

mod auto_save;
#[cfg(not(feature = "no-auto-reload"))]
mod auto_reload;
pub mod completion;
mod diagnostics;
mod document_colors;
mod signature_help;
mod snippet;
pub mod watcher;

pub fn setup(config: Arc<ArcSwap<Config>>) -> Handlers {
    events::register();

    let event_tx = completion::CompletionHandler::new(config).spawn();
    let signature_hints = SignatureHelpHandler::new().spawn();
    let auto_save = AutoSaveHandler::new().spawn();
    #[cfg(not(feature = "no-auto-reload"))]
    let auto_reload = match AutoReloadHandler::new() {
        Some(handler) => handler.start(),
        None => {
            let (tx, _rx) = tokio::sync::mpsc::channel(128);
            tx
        }
    };
    #[cfg(feature = "no-auto-reload")]
    let auto_reload = {
        let (tx, _rx) = tokio::sync::mpsc::channel(128);
        tx
    };
    #[cfg(not(feature = "no-auto-reload"))]
    {
        let auto_reload_clone = auto_reload.clone();
        watcher::spawn_config_watcher(auto_reload_clone);
    }
    let document_colors = DocumentColorsHandler::default().spawn();

    let handlers = Handlers {
        completions: helix_view::handlers::completion::CompletionHandler::new(event_tx),
        signature_hints,
        auto_save,
        auto_reload,
        document_colors,
    };

    helix_view::handlers::register_hooks(&handlers);
    completion::register_hooks(&handlers);
    signature_help::register_hooks(&handlers);
    auto_save::register_hooks(&handlers);
    #[cfg(not(feature = "no-auto-reload"))]
    auto_reload::register_hooks(&handlers);
    diagnostics::register_hooks(&handlers);
    snippet::register_hooks(&handlers);
    document_colors::register_hooks(&handlers);
    handlers
}
