use helix_core::{ChangeSet, Rope};
use helix_event::events;
use helix_lsp::LanguageServerId;

use crate::{ClientId, editor::Config, Document, DocumentId, Editor, ViewId};

use std::path::PathBuf;

pub enum FileModifiedType {
    NeedRescan,
    Paths(Vec<PathBuf>),
}

events! {
    DocumentDidOpen<'a> {
        editor: &'a mut Editor,
        doc: DocumentId
    }
    DocumentDidChange<'a> {
        doc: &'a mut Document,
        view: ViewId,
        old_text: &'a Rope,
        changes: &'a ChangeSet,
        ghost_transaction: bool
    }
    DocumentPathDidChange<'a> {
        doc: &'a mut Document,
        original_path: Option<PathBuf>
    }
    DocumentDidClose<'a> {
        editor: &'a mut Editor,
        doc: Document
    }
    FileModified {
        event: FileModifiedType
    }
    SelectionDidChange<'a> { doc: &'a mut Document, view: ViewId }
    DiagnosticsDidChange<'a> { editor: &'a mut Editor, doc: DocumentId }
    // called **after** a document loses focus (but not when its closed)
    DocumentFocusLost<'a> { editor: &'a mut Editor, client: ClientId, doc: DocumentId }

    LanguageServerInitialized<'a> {
        editor: &'a mut Editor,
        server_id: LanguageServerId
    }
    LanguageServerExited<'a> {
        editor: &'a mut Editor,
        server_id: LanguageServerId
    }

    // NOTE: this event is simple for now and is expected to change as the config system evolves.
    // Ideally it would say what changed.
    ConfigDidChange<'a> {
        editor: &'a mut Editor,
        old: &'a Config,
        new: &'a Config
    }
}
