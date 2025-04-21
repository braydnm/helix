use std::{collections::HashMap, path::PathBuf, sync, time::Duration};

use helix_event::register_hook;
use helix_view::{
    events::{
        DocumentDidClose, DocumentDidOpen, DocumentPathDidChange
    },
    handlers::{AutoReloadEvent, Handlers, FileEventKind, FileEvent},
    DocumentId, Editor,
};
use tokio::time::Instant;

use crate::{handlers::watcher::Watcher, job};
use super::watcher::RecommendedWatcher;

const AUTO_RELOAD_CREATE_SCAN: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub(super) struct AutoReloadHandler {
    watcher: RecommendedWatcher,
    path_doc_id: HashMap<PathBuf, DocumentId>,
    externally_modified_doc_ids: HashMap<DocumentId, FileEventKind>,
}

impl AutoReloadHandler {
    pub fn new() -> AutoReloadHandler {
        AutoReloadHandler {
            watcher: RecommendedWatcher::new().unwrap(),
            path_doc_id: HashMap::new(),
            externally_modified_doc_ids: HashMap::new(),
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
                self.watcher.watch(path).await;
            }
            AutoReloadEvent::DocumentClose(path) => {
                self.path_doc_id.remove(&path);
                self.watcher.unwatch(path).await;
            }
            AutoReloadEvent::DocumentPathChange(doc_id, original_path, new_path) => {
                if let Some(path) = original_path {
                    self.path_doc_id.remove(&path);
                    self.watcher.unwatch(path).await;
                }

                debug_assert!(!self.path_doc_id.values().any(|d| *d == doc_id));
                self.path_doc_id.insert(new_path.clone(), doc_id);
                self.watcher.watch(new_path).await;
            }
        }

        // TODO: Verify value
        self.watcher.commit();
    }

    async fn run(mut self, mut rx: tokio::sync::mpsc::Receiver<AutoReloadEvent>) {
        let creation_poll_sleep = tokio::time::sleep(AUTO_RELOAD_CREATE_SCAN);
        tokio::pin!(creation_poll_sleep);

        let mut watcher_fd = self.watcher.wait_fd();

        loop {
            tokio::select! {
                biased;
                guard = watcher_fd.readable_mut() => {
                    guard.unwrap().clear_ready_matching(tokio::io::Ready::READABLE);
                    while let Ok(Some(event)) = self.watcher.try_read_platform_event().await {
                        self.queue_modified_file(event);
                    }

                    self.watcher.commit().unwrap();
                },
                () = &mut creation_poll_sleep => {
                    let changed_files = self.watcher.poll_created_files().await.unwrap();
                    if !changed_files.is_empty() {
                        self.watcher.commit().unwrap();
                    }
                    for file in changed_files {
                        self.queue_modified_file(file);
                    }

                    creation_poll_sleep.as_mut().reset(Instant::now() + AUTO_RELOAD_CREATE_SCAN);
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

        job::dispatch_blocking_for_all_clients(
            move |editor: &mut Editor, clients: &ApplicationClients| {
                let scrolloff = editor.config().scrolloff;
                for (doc_id, update) in updated_doc_ids {
                    for client in clients.client_ids() {
                        let view_id = client_view!(editor, *client).id;
                        let doc = match editor.documents.get_mut(&doc_id) {
                            Some(d) => d,
                            None => {
                                log::warn!("Received file modify event for docID {doc_id}, which is closed. This may be a stale notify event");
                                return;
                            }
                        };

                        doc.handle_external_modify_event(&update);

                        if !doc.requires_reload() {
                            continue;
                        }

                if doc.has_conflicting_changes()
                    && editor
                        .tree
                        .try_get(editor.tree.focus)
                        .map_or(false, |view| view.doc == doc.id())
                {
                    editor.set_warning("file externally modified; :write! or :reload");
                    continue;
                } else if !doc.was_externally_modified() {
                    continue;
                }

                        if view_ids.is_empty() {
                            doc.ensure_view_init(view_id);
                            view_ids.push(view_id);
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
                                view.ensure_cursor_in_view(doc, scrolloff);
                            }
                        }
                    }
                }
            },
        );
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    // TODO: Handle renames
    let tx = handlers.auto_reload.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let path = event.editor.documents.get(&event.doc).unwrap().path();

        log::info!("Document opened!");

        if let Some(p) = path {
            helix_event::send_blocking(&tx, AutoReloadEvent::DocumentOpen(p.clone(), event.doc));
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
        helix_event::send_blocking(
            &tx,
            AutoReloadEvent::DocumentPathChange(
                event.doc.id(),
                event.original_path.clone(),
                event.doc.path().unwrap().clone(),
            ),
        );
        Ok(())
    });
}
