use std::{collections::HashMap, path::PathBuf, time::Duration};

use helix_event::register_hook;
use helix_view::{
    document::ExternalFileUpdate,
    events::{
        DocumentDidClose, DocumentDidOpen, DocumentPathDidChange, FileModified, FileModifiedType,
    },
    handlers::{AutoReloadEvent, Handlers},
    DocumentId, Editor,
};

use notify_debouncer_full::{
    notify::{
        event::CreateKind, event::ModifyKind, event::RemoveKind, EventKind, RecommendedWatcher,
        RecursiveMode,
    },
    DebounceEventResult, Debouncer, RecommendedCache,
};

use crate::{application::ApplicationClients, job};

const AUTO_RELOAD_DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub(super) struct AutoReloadHandler {
    watcher: Debouncer<RecommendedWatcher, RecommendedCache>,
    path_doc_id: HashMap<PathBuf, DocumentId>,
    externally_modified_doc_ids: HashMap<DocumentId, ExternalFileUpdate>,
}
fn handle_event(event: DebounceEventResult) {
    let mut events = match event {
        Ok(e) => e,
        Err(e) => {
            log::error!(
                "Received unexpected response from filesystem watcher: {:?}",
                e
            );
            return;
        }
    };

    let mut modified_paths: Vec<PathBuf> = Vec::new();

    for event in &mut events {
        match (event.need_rescan(), event.kind) {
            (true, _) => {
                log::warn!("Received a rescan event, this may lead to performance degredation if this occurs frequently");
                helix_event::dispatch(FileModified {
                    event: helix_view::events::FileModifiedType::NeedRescan,
                });
                return;
            }
            (
                false,
                invalid_event @ EventKind::Create(CreateKind::Folder)
                | invalid_event @ EventKind::Remove(RemoveKind::Folder),
            ) => {
                log::error!(
                    "Received an event that indicates a folder is being watched: {:?}",
                    invalid_event
                );
            }
            (false, EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_))) => {}
            (
                false,
                EventKind::Any
                | EventKind::Modify(_)
                | EventKind::Remove(RemoveKind::Any | RemoveKind::File | RemoveKind::Other)
                | EventKind::Create(_)
                | EventKind::Other,
            ) => {
                modified_paths.append(&mut event.paths);
            }
        };
    }

    helix_event::dispatch(FileModified {
        event: helix_view::events::FileModifiedType::Paths(modified_paths),
    });
}

impl AutoReloadHandler {
    pub fn new() -> AutoReloadHandler {
        AutoReloadHandler {
            watcher: notify_debouncer_full::new_debouncer(AUTO_RELOAD_DEBOUNCE, None, handle_event)
                .unwrap(),
            path_doc_id: HashMap::new(),
            externally_modified_doc_ids: HashMap::new(),
        }
    }

    fn queue_modified_file(&mut self, path: &PathBuf) {
        let doc_id = match self.path_doc_id.get(path) {
            Some(doc_id) => doc_id,
            None => {
                log::warn!(
                    "Got notified about {:?} but do not have doc ID in map",
                    path
                );
                return;
            }
        };

        if self.externally_modified_doc_ids.contains_key(doc_id) {
            return;
        }

        let status = match std::fs::metadata(path) {
            Ok(e) => ExternalFileUpdate::LastModified(e.modified().unwrap()),
            Err(_) => ExternalFileUpdate::DoesNotExit,
        };

        self.externally_modified_doc_ids
            .insert(doc_id.clone(), status);
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

                        let mut view_ids: Vec<_> = doc.selections().keys().cloned().collect();

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

impl helix_event::AsyncHook for AutoReloadHandler {
    type Event = AutoReloadEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        timeout: Option<tokio::time::Instant>,
    ) -> Option<tokio::time::Instant> {
        assert_eq!(timeout, None);

        match event {
            // TODO: Handle these better than just unwrapping
            AutoReloadEvent::DocumentOpen(path, document_id) => {
                debug_assert!(!path.clone().as_path().is_dir());
                self.path_doc_id.insert(path.clone(), document_id);
                self.watcher
                    .watch(path, RecursiveMode::NonRecursive)
                    .unwrap();
            }
            AutoReloadEvent::DocumentClose(path) => {
                self.path_doc_id.remove(&path);
                self.watcher.unwatch(path).unwrap();
            }
            AutoReloadEvent::DocumentPathChange(doc_id, original_path, new_path) => {
                if let Some(path) = original_path {
                    self.path_doc_id.remove(&path);
                    self.watcher.unwatch(path).unwrap();
                }

                debug_assert!(!self.path_doc_id.values().any(|d| *d == doc_id));
                self.path_doc_id.insert(new_path.clone(), doc_id);
                self.watcher
                    .watch(new_path, RecursiveMode::NonRecursive)
                    .unwrap();
            }
            AutoReloadEvent::DocumentsModified(paths) => {
                for path in paths {
                    self.queue_modified_file(&path);
                }
            }
            AutoReloadEvent::ReloadAllDocuments => {
                let paths: Vec<PathBuf> = self.path_doc_id.keys().cloned().collect();
                for path in paths {
                    self.queue_modified_file(&path);
                }
            }
        };

        self.handle_external_updates();
        None
    }

    fn finish_debounce(&mut self) {
        unreachable!();
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    // TODO: Handle renames
    let tx = handlers.auto_reload.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        let path = event.editor.documents.get(&event.doc).unwrap().path();

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
    register_hook!(move |event: &mut FileModified| {
        // TODO: Add To/From traits
        let internal_event = match &event.event {
            FileModifiedType::Paths(p) => AutoReloadEvent::DocumentsModified(p.clone()),
            FileModifiedType::NeedRescan => AutoReloadEvent::ReloadAllDocuments,
        };

        helix_event::send_blocking(&tx, internal_event);
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
