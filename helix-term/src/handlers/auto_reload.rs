use std::{collections::HashMap, path::PathBuf};

use helix_event::register_hook;
use helix_view::{
    editor::ConfigEvent,
    events::{DocumentDidClose, DocumentDidOpen, DocumentPathDidChange},
    handlers::{AutoReloadEvent, FileEvent, FileEventKind, Handlers},
    view_mut, DocumentId, Editor,
};

use crate::job;
use super::watcher::WatchmanWatcher;


#[derive(Debug)]
pub(super) struct AutoReloadHandler {
    watcher: WatchmanWatcher,
    path_doc_id: HashMap<PathBuf, DocumentId>,
    externally_modified_doc_ids: HashMap<DocumentId, FileEventKind>,
}

impl AutoReloadHandler {
    pub fn new() -> Option<AutoReloadHandler> {
        match WatchmanWatcher::new() {
            Ok(watcher) => Some(AutoReloadHandler {
                watcher,
                path_doc_id: HashMap::new(),
                externally_modified_doc_ids: HashMap::new(),
            }),
            Err(err) => {
                log::error!("Failed to create file watcher: {}", err);
                None
            }
        }
    }

    pub fn start(self) -> tokio::sync::mpsc::Sender<AutoReloadEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(self.run(rx));
        }
        tx
    }

    async fn handle_auto_reload_event(&mut self, event: AutoReloadEvent) {
        match event {
            // TODO: Handle these better than just unwrapping
            AutoReloadEvent::DocumentOpen(path, document_id) => {
                log::info!("Document opened!");
                debug_assert!(!path.is_dir());
                self.path_doc_id.insert(path.clone(), document_id);
                let _ = self.watcher.watch(path).await;
            }
            AutoReloadEvent::DocumentClose(path) => {
                self.path_doc_id.remove(&path);
                let _ = self.watcher.unwatch(path).await;
            }
            AutoReloadEvent::DocumentPathChange(doc_id, original_path, new_path) => {
                if let Some(path) = original_path {
                    self.path_doc_id.remove(&path);
                    let _ = self.watcher.unwatch(path).await;
                }

                self.path_doc_id.insert(new_path.clone(), doc_id);
                let _ = self.watcher.watch(new_path).await;
            }
            AutoReloadEvent::ConfigChanged => {
                job::dispatch_blocking_editor_only(move |editor: &mut Editor| {
                    let _ = editor.config_events.0.send(ConfigEvent::Refresh);
                });
            }
        }

        let _ = self.watcher.commit();
    }

    async fn run(mut self, mut rx: tokio::sync::mpsc::Receiver<AutoReloadEvent>) {
        let mut watcher_fd = self.watcher.wait_fd();

        loop {
            tokio::select! {
                biased;
                guard = watcher_fd.readable_mut() => {
                    match guard {
                        Ok(mut guard) => {
                            guard.clear_ready_matching(tokio::io::Ready::READABLE);
                            while let Ok(Some(event)) = self.watcher.try_read_platform_event().await {
                                self.queue_modified_file(event);
                            }

                            if let Err(err) = self.watcher.commit() {
                                log::error!("Failed to commit watcher changes: {}", err);
                            }
                        }
                        Err(err) => {
                            log::error!("Failed to get readable guard: {}", err);
                        }
                    }
                },
                event = rx.recv() => {
                    if let Some(event) = event {
                        self.handle_auto_reload_event(event).await;
                    }
                }
            }

            if !self.externally_modified_doc_ids.is_empty() {
                self.handle_external_updates();
            }
        }
    }

    fn queue_modified_file(&mut self, event: FileEvent) {
        let doc_id = match self.path_doc_id.get(&event.path) {
            Some(doc_id) => doc_id,
            None => {
                log::warn!(
                    "Got notified about {:?} but do not have doc ID in map",
                    event.path
                );
                return;
            }
        };

        self.externally_modified_doc_ids
            .insert(doc_id.clone(), event.kind);
    }

    fn handle_external_updates(&mut self) {
        let updated_doc_ids = std::mem::take(&mut self.externally_modified_doc_ids);

        if updated_doc_ids.is_empty() {
            return;
        }

        job::dispatch_blocking_editor_only(move |editor: &mut Editor| {
            let scrolloff = editor.config().scrolloff;
            let focus_view_id = Some(editor.tree.focus);

            for (doc_id, update) in updated_doc_ids {
                let doc = match editor.documents.get_mut(&doc_id) {
                    Some(d) => d,
                    None => {
                        log::warn!("Received file modify event for docID {doc_id}, which is closed. This may be a stale notify event");
                        continue;
                    }
                };

                doc.handle_external_modify_event(&update);

                if !doc.requires_reload() {
                    continue;
                }

                let focus_view = focus_view_id.and_then(|id| editor.tree.try_get(id));
                if doc.has_conflicting_changes()
                    && focus_view.is_some_and(|view| view.doc == doc.id())
                {
                    editor.set_warning("file externally modified; :write! or :reload");
                    continue;
                } else if !doc.was_externally_modified() {
                    continue;
                }

                let mut view_ids: Vec<_> = doc.selections().keys().cloned().collect();

                if view_ids.is_empty() {
                    let Some(focus_view_id) = focus_view_id else {
                        continue;
                    };
                    doc.ensure_view_init(focus_view_id);
                    view_ids.push(focus_view_id);
                };

                let view = view_mut!(editor, view_ids[0]);
                view.sync_changes(doc);

                if let Err(error) = doc.reload(view, &editor.diff_providers) {
                    editor.set_error(format!("{}", error));
                    continue;
                }

                if let Some(path) = doc.path() {
                    editor
                        .language_servers
                        .file_event_handler
                        .file_changed(path.clone());
                }

                for view_id in view_ids {
                    let view = view_mut!(editor, view_id);
                    if view.doc.eq(&doc_id) {
                        let doc = editor.documents.get_mut(&doc_id).unwrap();
                        view.ensure_cursor_in_view(doc, scrolloff);
                    }
                }
            }
        });
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    // TODO: Handle renames
    let tx = handlers.auto_reload.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        match event.editor.documents.get(&event.doc) {
            Some(doc) => {
                let path = doc.path();
                log::info!("Document opened!");
                if let Some(p) = path {
                    helix_event::send_blocking(&tx, AutoReloadEvent::DocumentOpen(p.clone(), event.doc));
                }
            }
            None => {
                log::warn!("DocumentDidOpen event received for non-existent document ID: {:?}", event.doc);
            }
        }
        Ok(())
    });

    let tx = handlers.auto_reload.clone();
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        let path = event.doc.path();

        if let Some(p) = path {
            helix_event::send_blocking(&tx, AutoReloadEvent::DocumentClose(p.clone()));
        }
        Ok(())
    });

    let tx = handlers.auto_reload.clone();
    register_hook!(move |event: &mut DocumentPathDidChange<'_>| {
        match event.doc.path() {
            Some(new_path) => {
                helix_event::send_blocking(
                    &tx,
                    AutoReloadEvent::DocumentPathChange(
                        event.doc.id(),
                        event.original_path.clone(),
                        new_path.clone(),
                    ),
                );
            }
            None => {
                log::warn!("DocumentPathDidChange event received but document has no path");
            }
        }
        Ok(())
    });

}
