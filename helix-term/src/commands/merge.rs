use std::fs;
use std::path::Path;

use helix_core::{Rope, Transaction};
use helix_view::editor::Action;
use helix_view::input::KeyEvent;
use helix_view::merge::{self, MergeRole, MergeSession, MergeSide};
use helix_view::Document;

use crate::commands::Context;
use crate::ui::{self, overlay::overlaid, PickerColumn};

pub fn merge_conflict_picker(cx: &mut Context) {
    tear_down_session_on_editor(cx.editor);

    let cwd = helix_stdx::env::current_working_dir();
    if !cwd.exists() {
        cx.editor
            .set_error("Current working directory does not exist");
        return;
    }

    let columns = [PickerColumn::new(
        "path",
        |path: &std::path::PathBuf, data: &std::path::PathBuf| {
            path.strip_prefix(data)
                .unwrap_or(path)
                .display()
                .to_string()
                .into()
        },
    )];

    let picker = ui::Picker::new(
        columns,
        0,
        [],
        cwd.clone(),
        |cx, path: &std::path::PathBuf, _action| {
            open_merge_session_on_editor(cx.editor, path);
        },
    );
    let injector = picker.injector();

    let walk_root = cwd.clone();
    std::thread::spawn(move || {
        let conflicts = merge::find_jj_conflicts(&walk_root);
        for path in conflicts {
            if injector.push(path).is_err() {
                break;
            }
        }
    });

    cx.push_layer(Box::new(overlaid(picker)));
}

fn open_merge_session_on_editor(editor: &mut helix_view::Editor, path: &Path) {
    let content = match fs::read(path) {
        Ok(c) => c,
        Err(e) => {
            editor.set_error(format!("Failed to read {}: {}", path.display(), e));
            return;
        }
    };

    let parsed = match merge::parse_conflict_file(&content) {
        Some(p) => p,
        None => {
            editor.set_error(format!("No conflicts found in {}", path.display()));
            return;
        }
    };

    let base_text_bytes = parsed.base_text.as_bytes().to_vec();
    let num_sides = parsed.num_sides;
    let left_count = num_sides / 2;

    let mut sides = Vec::with_capacity(num_sides);

    // Layout: [left_sides... | base | right_sides...]

    // Left sides
    let first_side_doc_id = create_merge_doc(editor, &parsed.side_texts[0], path);
    {
        let doc = editor.documents.get_mut(&first_side_doc_id).unwrap();
        doc.set_diff_base(base_text_bytes.clone());
    }
    editor.switch(first_side_doc_id, Action::Replace);
    let first_view_id = editor.tree.focus;
    sides.push(MergeSide {
        doc_id: first_side_doc_id,
        view_id: first_view_id,
        role: MergeRole::Side(0),
    });

    for i in 1..left_count {
        let side_doc_id = create_merge_doc(editor, &parsed.side_texts[i], path);
        {
            let doc = editor.documents.get_mut(&side_doc_id).unwrap();
            doc.set_diff_base(base_text_bytes.clone());
        }
        editor.switch(side_doc_id, Action::VerticalSplitAlwaysInWindow);
        let view_id = editor.tree.focus;
        sides.push(MergeSide {
            doc_id: side_doc_id,
            view_id,
            role: MergeRole::Side(i),
        });
    }

    // Base (center)
    let base_doc_id = create_merge_doc(editor, &parsed.base_text, path);
    editor.switch(base_doc_id, Action::VerticalSplitAlwaysInWindow);
    let base_view_id = editor.tree.focus;

    // Right sides
    for i in left_count..num_sides {
        let side_doc_id = create_merge_doc(editor, &parsed.side_texts[i], path);
        {
            let doc = editor.documents.get_mut(&side_doc_id).unwrap();
            doc.set_diff_base(base_text_bytes.clone());
        }
        editor.switch(side_doc_id, Action::VerticalSplitAlwaysInWindow);
        let view_id = editor.tree.focus;
        sides.push(MergeSide {
            doc_id: side_doc_id,
            view_id,
            role: MergeRole::Side(i),
        });
    }

    editor.tree.focus = base_view_id;

    let original_regions = parsed.conflict_regions.clone();
    let original_base_text = parsed.base_text;

    // Highlight base-pane edits against the pristine parsed base so the user
    // can see exactly which regions they (or a side accept) have changed.
    {
        let base_doc = editor.documents.get_mut(&base_doc_id).unwrap();
        base_doc.set_diff_base(original_base_text.as_bytes().to_vec());
    }

    editor.merge_session = Some(MergeSession {
        original_path: path.to_path_buf(),
        original_doc_id: None,
        sides,
        base: MergeSide {
            doc_id: base_doc_id,
            view_id: base_view_id,
            role: MergeRole::Base,
        },
        conflict_regions: parsed.conflict_regions,
        num_sides: parsed.num_sides,
        conflict_marker_len: parsed.conflict_marker_len,
        original_base_text,
        original_side_texts: parsed.side_texts,
        original_regions,
    });

    let unresolved = editor
        .merge_session
        .as_ref()
        .unwrap()
        .unresolved_count();
    editor.set_status(format!("Merge view: {} conflicts", unresolved));
}

fn create_merge_doc(
    editor: &mut helix_view::Editor,
    text: &str,
    original_path: &Path,
) -> helix_view::DocumentId {
    let rope = Rope::from_str(text);
    let mut doc = Document::from(rope, None, editor.config.clone(), editor.syn_loader.clone());

    let ext = original_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let prefix = original_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("merge");

    if let Ok(tmp) = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(&format!(".{ext}"))
        .tempfile()
    {
        let tmp_path = tmp.path().to_path_buf();
        doc.set_path(Some(&tmp_path));
        let loader = editor.syn_loader.load();
        doc.detect_language(&loader);
        doc.set_path(None);
        let _ = fs::remove_file(&tmp_path);
    }

    editor.new_document(doc)
}

pub fn tear_down_session_on_editor(editor: &mut helix_view::Editor) {
    let session = match editor.merge_session.take() {
        Some(s) => s,
        None => return,
    };

    let doc_ids = session.all_doc_ids();
    for doc_id in &doc_ids {
        let _ = editor.close_document(*doc_id, true);
    }
}

/// Refresh the diff_base bytes on every side document to match the live base
/// text. Side panes show their own content vs base, so they need to be re-told
/// after every base mutation (accept, undo, redo, direct edit).
fn refresh_side_diff_bases(editor: &mut helix_view::Editor) {
    let base_doc_id = match editor.merge_session.as_ref() {
        Some(s) => s.base.doc_id,
        None => return,
    };
    let base_bytes = match editor.documents.get(&base_doc_id) {
        Some(d) => {
            let mut bytes = Vec::new();
            for chunk in d.text().chunks() {
                bytes.extend_from_slice(chunk.as_bytes());
            }
            bytes
        }
        None => return,
    };
    let side_doc_ids: Vec<helix_view::DocumentId> = editor
        .merge_session
        .as_ref()
        .unwrap()
        .sides
        .iter()
        .map(|s| s.doc_id)
        .collect();
    for doc_id in &side_doc_ids {
        if let Some(doc) = editor.documents.get_mut(doc_id) {
            doc.set_diff_base(base_bytes.clone());
        }
    }
}

/// Recompute the live `conflict_regions` from the current base document text.
/// Called at the top of every merge command so undo/redo, accept_side, and
/// direct edits all converge on the same source of truth.
pub fn refresh_regions(editor: &mut helix_view::Editor) {
    let Some(session) = editor.merge_session.as_ref() else {
        return;
    };
    let base_doc_id = session.base.doc_id;
    let Some(base_doc) = editor.documents.get(&base_doc_id) else {
        return;
    };
    let current_base = base_doc.text().to_string();
    let regions = merge::derive_regions(
        &current_base,
        &session.original_base_text,
        &session.original_side_texts,
        &session.original_regions,
    );
    if let Some(session) = editor.merge_session.as_mut() {
        session.conflict_regions = regions;
    }
}

pub fn tear_down_merge_session(cx: &mut Context) {
    tear_down_session_on_editor(cx.editor);
}

/// Write the current base document text to the original path, tear down the
/// merge session, and reopen the file as a regular document. Used by both the
/// auto-write-on-full-resolution path in `accept_side` and the `:w` save flow
/// in `merge_save` once all conflicts are gone.
pub fn finalize_merge_session(
    editor: &mut helix_view::Editor,
    target_path: &Path,
) -> std::io::Result<()> {
    let base_doc_id = match editor.merge_session.as_ref() {
        Some(s) => s.base.doc_id,
        None => return Ok(()),
    };
    let resolved_text = editor
        .documents
        .get(&base_doc_id)
        .map(|d| d.text().to_string())
        .unwrap_or_default();

    fs::write(target_path, resolved_text.as_bytes())?;

    tear_down_session_on_editor(editor);

    if let Err(e) = editor.open(target_path, Action::Replace) {
        log::warn!("merge: failed to reopen {}: {:?}", target_path.display(), e);
    }
    Ok(())
}

pub fn goto_next_conflict(cx: &mut Context) {
    refresh_regions(cx.editor);
    navigate_conflict(cx, Direction::Forward);
}

pub fn goto_prev_conflict(cx: &mut Context) {
    refresh_regions(cx.editor);
    navigate_conflict(cx, Direction::Backward);
}

enum Direction {
    Forward,
    Backward,
}

fn navigate_conflict(cx: &mut Context, direction: Direction) {
    let session = match &cx.editor.merge_session {
        Some(s) => s,
        None => {
            cx.editor.set_status("No active merge session");
            return;
        }
    };

    let (view, doc) = helix_view::current!(cx.editor);
    let cursor_line = doc
        .text()
        .char_to_line(doc.selection(view.id).primary().cursor(doc.text().slice(..)));

    let role = session.find_role(doc.id()).unwrap_or(MergeRole::Base);
    let unresolved: Vec<usize> = session
        .conflict_regions
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.resolved)
        .map(|(i, _)| i)
        .collect();

    if unresolved.is_empty() {
        cx.editor.set_status("All conflicts resolved");
        return;
    }

    let current_region_line = |idx: usize| -> usize {
        session
            .lines_for_role(&session.conflict_regions[idx], role)
            .start
    };

    let target_idx = match direction {
        Direction::Forward => unresolved
            .iter()
            .find(|&&i| current_region_line(i) > cursor_line)
            .or(unresolved.first())
            .copied()
            .unwrap(),
        Direction::Backward => unresolved
            .iter()
            .rev()
            .find(|&&i| current_region_line(i) < cursor_line)
            .or(unresolved.last())
            .copied()
            .unwrap(),
    };

    let scrolloff = cx.editor.config().scrolloff;

    let all_sides: Vec<(helix_view::DocumentId, helix_view::ViewId, MergeRole)> = {
        let session = cx.editor.merge_session.as_ref().unwrap();
        session
            .all_sides()
            .map(|s| (s.doc_id, s.view_id, s.role))
            .collect()
    };

    for (doc_id, view_id, side_role) in &all_sides {
        let region = &cx.editor.merge_session.as_ref().unwrap().conflict_regions[target_idx];
        let line_range = cx
            .editor
            .merge_session
            .as_ref()
            .unwrap()
            .lines_for_role(region, *side_role);
        let target_line = line_range.start;

        let doc = match cx.editor.documents.get_mut(doc_id) {
            Some(d) => d,
            None => continue,
        };
        if !cx.editor.tree.contains(*view_id) {
            continue;
        }
        let pos = doc
            .text()
            .line_to_char(target_line.min(doc.text().len_lines().saturating_sub(1)));
        doc.set_selection(*view_id, helix_core::Selection::point(pos));

        let view = cx.editor.tree.get_mut(*view_id);
        view.ensure_cursor_in_view(doc, scrolloff);
    }

    let remaining = cx
        .editor
        .merge_session
        .as_ref()
        .unwrap()
        .unresolved_count();
    cx.editor
        .set_status(format!("Conflict {} of {}", target_idx + 1, remaining));
}

pub fn merge_accept(cx: &mut Context) {
    let session = match &cx.editor.merge_session {
        Some(s) => s,
        None => {
            cx.editor.set_status("No active merge session");
            return;
        }
    };

    let (_view, doc) = helix_view::current!(cx.editor);
    let role = session.find_role(doc.id());

    match role {
        Some(MergeRole::Base) => {
            let num_sides = session.num_sides;
            let side_labels: Vec<String> = (0..num_sides)
                .map(|i| format!("{}", i + 1))
                .collect();
            cx.editor.set_status(format!(
                "Accept from side [{}]:",
                side_labels.join("/")
            ));
            cx.on_next_key(move |cx, event| {
                accept_from_key(cx, event);
            });
        }
        Some(MergeRole::Side(side_idx)) => {
            accept_side(cx, side_idx);
        }
        None => {
            cx.editor.set_status("Not in a merge view");
        }
    }
}

/// True iff a merge session is active and the focused document belongs to it.
fn in_merge_buffer(editor: &helix_view::Editor) -> bool {
    let Some(session) = editor.merge_session.as_ref() else {
        return false;
    };
    let doc_id = helix_view::doc!(editor).id();
    session.find_role(doc_id).is_some()
}

/// Undo the *base* document regardless of which pane is focused, then refresh
/// derived regions and side diff bases so the next render reflects the rolled
/// back state.
enum HistoryDirection {
    Undo,
    Redo,
}

fn step_history(
    cx: &mut Context,
    doc_id: helix_view::DocumentId,
    view_id: helix_view::ViewId,
    count: usize,
    dir: &HistoryDirection,
) -> usize {
    if !cx.editor.tree.contains(view_id) {
        return 0;
    }
    let view = cx.editor.tree.get_mut(view_id);
    let doc = match cx.editor.documents.get_mut(&doc_id) {
        Some(d) => d,
        None => return 0,
    };
    let mut steps = 0;
    for _ in 0..count {
        let advanced = match dir {
            HistoryDirection::Undo => doc.undo(view),
            HistoryDirection::Redo => doc.redo(view),
        };
        if !advanced {
            break;
        }
        steps += 1;
    }
    steps
}

fn merge_history(cx: &mut Context, dir: HistoryDirection) {
    let count = cx.count();
    let session = match cx.editor.merge_session.as_ref() {
        Some(s) => s,
        None => return,
    };
    let base_doc_id = session.base.doc_id;
    let base_view_id = session.base.view_id;
    let session_doc_ids = session.all_doc_ids();

    let focused_view_id = cx.editor.tree.focus;
    let focused_doc_id = cx
        .editor
        .tree
        .try_get(focused_view_id)
        .map(|v| v.doc);

    // Prefer the focused merge pane (so a user editing a side can undo their
    // own typing). Fall back to base when the focused pane is non-merge or has
    // nothing left to roll back, so accept_side edits remain undoable from any
    // pane.
    let primary = match focused_doc_id {
        Some(id) if session_doc_ids.contains(&id) => Some((id, focused_view_id)),
        _ => None,
    };

    let mut total = 0;
    if let Some((doc_id, view_id)) = primary {
        total += step_history(cx, doc_id, view_id, count, &dir);
    }
    if total < count && primary.map(|(d, _)| d) != Some(base_doc_id) {
        let remaining = count - total;
        total += step_history(cx, base_doc_id, base_view_id, remaining, &dir);
    }

    if total == 0 {
        let msg = match dir {
            HistoryDirection::Undo => "Already at oldest change",
            HistoryDirection::Redo => "Already at newest change",
        };
        cx.editor.set_status(msg);
    }

    refresh_regions(cx.editor);
    refresh_side_diff_bases(cx.editor);
}

pub fn merge_undo(cx: &mut Context) {
    merge_history(cx, HistoryDirection::Undo);
}

pub fn merge_redo(cx: &mut Context) {
    merge_history(cx, HistoryDirection::Redo);
}

/// In any merge pane, redirect undo to the base document so the user doesn't
/// have to focus base first. Outside merge buffers, behaves like default undo.
pub fn merge_undo_or_undo(cx: &mut Context) {
    if in_merge_buffer(cx.editor) {
        merge_undo(cx);
    } else {
        super::undo(cx);
    }
}

pub fn merge_redo_or_redo(cx: &mut Context) {
    if in_merge_buffer(cx.editor) {
        merge_redo(cx);
    } else {
        super::redo(cx);
    }
}

/// In a merge buffer, dispatches to `merge_focus` so the next keystroke
/// (b/0 or 1..N) jumps to the matching pane. Outside merge buffers, behaves
/// exactly like the default `select_regex` so the keymap stays unchanged for
/// everyone else.
pub fn merge_focus_or_select_regex(cx: &mut Context) {
    if in_merge_buffer(cx.editor) {
        merge_focus(cx);
    } else {
        super::select_regex(cx);
    }
}

pub fn merge_focus(cx: &mut Context) {
    let session = match &cx.editor.merge_session {
        Some(s) => s,
        None => {
            cx.editor.set_status("No active merge session");
            return;
        }
    };
    let num_sides = session.num_sides;
    let mut labels = vec!["b".to_string()];
    for i in 0..num_sides {
        labels.push(format!("{}", i + 1));
    }
    cx.editor
        .set_status(format!("Focus pane [{}]:", labels.join("/")));
    cx.on_next_key(move |cx, event| {
        focus_from_key(cx, event);
    });
}

fn focus_from_key(cx: &mut Context, event: KeyEvent) {
    let ch = match event {
        KeyEvent {
            code: helix_view::keyboard::KeyCode::Char(c),
            ..
        } => c,
        _ => {
            cx.editor.set_status("Cancelled");
            return;
        }
    };

    let session = match &cx.editor.merge_session {
        Some(s) => s,
        None => return,
    };

    let target_view_id = if ch == 'b' || ch == 'B' || ch == '0' {
        session.base.view_id
    } else if let Some(n) = ch.to_digit(10) {
        let idx = n.saturating_sub(1) as usize;
        if idx >= session.num_sides {
            cx.editor
                .set_status(format!("Side {} does not exist", n));
            return;
        }
        session.sides[idx].view_id
    } else {
        cx.editor.set_status("Cancelled");
        return;
    };

    if !cx.editor.tree.contains(target_view_id) {
        cx.editor.set_status("Pane no longer exists");
        return;
    }

    // Capture the cursor line in the source pane so we can carry it across.
    let source_view_id = cx.editor.tree.focus;
    let source_line = cx.editor.tree.try_get(source_view_id).and_then(|view| {
        let doc = cx.editor.documents.get(&view.doc)?;
        let cursor = doc.selection(view.id).primary().cursor(doc.text().slice(..));
        Some(doc.text().char_to_line(cursor))
    });

    cx.editor.focus(target_view_id);

    if let Some(line) = source_line {
        let target_doc_id = cx.editor.tree.get(target_view_id).doc;
        let scrolloff = cx.editor.config().scrolloff;
        let target_doc = cx
            .editor
            .documents
            .get_mut(&target_doc_id)
            .expect("target doc must exist");
        let max_line = target_doc.text().len_lines().saturating_sub(1);
        let clamped = line.min(max_line);
        let pos = target_doc.text().line_to_char(clamped);
        target_doc.set_selection(target_view_id, helix_core::Selection::point(pos));
        let view = cx.editor.tree.get_mut(target_view_id);
        view.ensure_cursor_in_view(target_doc, scrolloff);
    }
}

pub fn merge_push(cx: &mut Context) {
    let session = match &cx.editor.merge_session {
        Some(s) => s,
        None => {
            cx.editor.set_status("No active merge session");
            return;
        }
    };

    let (_view, doc) = helix_view::current!(cx.editor);
    match session.find_role(doc.id()) {
        Some(MergeRole::Side(side_idx)) => {
            accept_side(cx, side_idx);
        }
        Some(MergeRole::Base) => {
            cx.editor
                .set_status("Already in base; use merge_accept to pull from a side");
        }
        None => {
            cx.editor.set_status("Not in a merge view");
        }
    }
}

fn accept_from_key(cx: &mut Context, event: KeyEvent) {
    let ch = match event {
        KeyEvent {
            code: helix_view::keyboard::KeyCode::Char(c),
            ..
        } => c,
        _ => {
            cx.editor.set_status("Cancelled");
            return;
        }
    };

    let side_idx = match ch.to_digit(10) {
        Some(n) if n >= 1 => (n - 1) as usize,
        _ => {
            cx.editor.set_status("Invalid side number");
            return;
        }
    };

    let session = match &cx.editor.merge_session {
        Some(s) => s,
        None => return,
    };

    if side_idx >= session.num_sides {
        cx.editor.set_status(format!(
            "Side {} does not exist (max {})",
            side_idx + 1,
            session.num_sides
        ));
        return;
    }

    accept_side(cx, side_idx);
}

fn accept_side(cx: &mut Context, side_idx: usize) {
    refresh_regions(cx.editor);

    let session = match &cx.editor.merge_session {
        Some(s) => s,
        None => return,
    };

    let (view, doc) = helix_view::current!(cx.editor);
    let doc_id = doc.id();
    let view_id = view.id;
    let role = session.find_role(doc_id).unwrap_or(MergeRole::Base);

    let cursor_line = doc
        .text()
        .char_to_line(doc.selection(view_id).primary().cursor(doc.text().slice(..)));

    let region_idx = find_conflict_at_line(session, role, cursor_line);
    let region_idx = match region_idx {
        Some(i) => i,
        None => {
            cx.editor.set_status("No conflict at cursor");
            return;
        }
    };

    if session.conflict_regions[region_idx].resolved {
        cx.editor.set_status("Conflict already resolved");
        return;
    }

    let side_doc_id = session.sides[side_idx].doc_id;
    let base_doc_id = session.base.doc_id;
    let base_view_id = session.base.view_id;
    let region = session.conflict_regions[region_idx].clone();

    let side_range = &region.side_lines[side_idx];
    let side_text = {
        let side_doc = cx.editor.documents.get(&side_doc_id).unwrap();
        let start = side_doc.text().line_to_char(side_range.start);
        let end = if side_range.end >= side_doc.text().len_lines() {
            side_doc.text().len_chars()
        } else {
            side_doc.text().line_to_char(side_range.end)
        };
        side_doc.text().slice(start..end).to_string()
    };

    let base_doc = cx.editor.documents.get_mut(&base_doc_id).unwrap();
    let base_start = base_doc
        .text()
        .line_to_char(region.base_lines.start);
    let base_end = if region.base_lines.end >= base_doc.text().len_lines() {
        base_doc.text().len_chars()
    } else {
        base_doc.text().line_to_char(region.base_lines.end)
    };

    let transaction = Transaction::change(
        base_doc.text(),
        [(base_start, base_end, Some(side_text.into()))].into_iter(),
    );
    base_doc.apply(&transaction, base_view_id);

    refresh_regions(cx.editor);
    refresh_side_diff_bases(cx.editor);

    let remaining = cx
        .editor
        .merge_session
        .as_ref()
        .unwrap()
        .unresolved_count();

    if remaining == 0 {
        // Keep the session alive even when all conflicts are resolved so the
        // user can undo accepts up until they explicitly `:w` the file.
        cx.editor.set_status(format!(
            "Accepted side {}. All conflicts resolved \u{2014} :w to write.",
            side_idx + 1
        ));
    } else {
        cx.editor.set_status(format!(
            "Accepted side {}. {} conflicts remaining",
            side_idx + 1,
            remaining
        ));
    }
}

fn find_conflict_at_line(
    session: &MergeSession,
    role: MergeRole,
    cursor_line: usize,
) -> Option<usize> {
    // Exact match first
    if let Some(idx) = session
        .conflict_regions
        .iter()
        .enumerate()
        .find(|(_, region)| {
            let range = session.lines_for_role(region, role);
            cursor_line >= range.start && cursor_line < range.end
        })
        .map(|(i, _)| i)
    {
        return Some(idx);
    }

    // Nearest unresolved conflict
    session
        .conflict_regions
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.resolved)
        .min_by_key(|(_, region)| {
            let range = session.lines_for_role(region, role);
            let mid = (range.start + range.end) / 2;
            (cursor_line as isize - mid as isize).unsigned_abs()
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::merge::{ConflictRegion, MergeRole, MergeSession, MergeSide};
    use helix_view::{DocumentId, ViewId};
    use std::path::PathBuf;

    fn make_session(regions: Vec<ConflictRegion>) -> MergeSession {
        MergeSession {
            original_path: PathBuf::from("/test"),
            original_doc_id: None,
            sides: vec![
                MergeSide {
                    doc_id: DocumentId::default(),
                    view_id: ViewId::default(),
                    role: MergeRole::Side(0),
                },
                MergeSide {
                    doc_id: DocumentId::default(),
                    view_id: ViewId::default(),
                    role: MergeRole::Side(1),
                },
            ],
            base: MergeSide {
                doc_id: DocumentId::default(),
                view_id: ViewId::default(),
                role: MergeRole::Base,
            },
            conflict_regions: regions.clone(),
            num_sides: 2,
            conflict_marker_len: 7,
            original_base_text: String::new(),
            original_side_texts: vec![String::new(), String::new()],
            original_regions: regions,
        }
    }

    #[test]
    fn find_conflict_exact_match_base() {
        let session = make_session(vec![ConflictRegion {
            base_lines: 5..10,
            side_lines: vec![5..8, 5..12],
            resolved: false,
        }]);
        assert_eq!(find_conflict_at_line(&session, MergeRole::Base, 5), Some(0));
        assert_eq!(find_conflict_at_line(&session, MergeRole::Base, 7), Some(0));
        assert_eq!(find_conflict_at_line(&session, MergeRole::Base, 9), Some(0));
    }

    #[test]
    fn find_conflict_exact_match_side() {
        let session = make_session(vec![ConflictRegion {
            base_lines: 5..10,
            side_lines: vec![5..8, 5..12],
            resolved: false,
        }]);
        assert_eq!(
            find_conflict_at_line(&session, MergeRole::Side(0), 5),
            Some(0)
        );
        assert_eq!(
            find_conflict_at_line(&session, MergeRole::Side(0), 7),
            Some(0)
        );
        // line 8 is outside 5..8 (exclusive end), but nearest-unresolved fallback finds it
        assert_eq!(
            find_conflict_at_line(&session, MergeRole::Side(0), 8),
            Some(0)
        );
        assert_eq!(
            find_conflict_at_line(&session, MergeRole::Side(1), 11),
            Some(0)
        );
    }

    #[test]
    fn find_conflict_outside_range_finds_nearest() {
        let session = make_session(vec![
            ConflictRegion {
                base_lines: 5..8,
                side_lines: vec![5..8, 5..8],
                resolved: false,
            },
            ConflictRegion {
                base_lines: 20..25,
                side_lines: vec![20..25, 20..25],
                resolved: false,
            },
        ]);
        assert_eq!(find_conflict_at_line(&session, MergeRole::Base, 0), Some(0));
        assert_eq!(
            find_conflict_at_line(&session, MergeRole::Base, 100),
            Some(1)
        );
    }

    #[test]
    fn find_conflict_skips_resolved_for_nearest() {
        let session = make_session(vec![
            ConflictRegion {
                base_lines: 5..8,
                side_lines: vec![5..8, 5..8],
                resolved: true,
            },
            ConflictRegion {
                base_lines: 20..25,
                side_lines: vec![20..25, 20..25],
                resolved: false,
            },
        ]);
        assert_eq!(find_conflict_at_line(&session, MergeRole::Base, 0), Some(1));
    }

    #[test]
    fn find_conflict_exact_match_on_resolved_still_returns_it() {
        let session = make_session(vec![ConflictRegion {
            base_lines: 5..8,
            side_lines: vec![5..8, 5..8],
            resolved: true,
        }]);
        assert_eq!(find_conflict_at_line(&session, MergeRole::Base, 6), Some(0));
    }

    #[test]
    fn find_conflict_no_regions() {
        let session = make_session(vec![]);
        assert_eq!(find_conflict_at_line(&session, MergeRole::Base, 0), None);
    }

    #[test]
    fn find_conflict_all_resolved_no_exact_match() {
        let session = make_session(vec![ConflictRegion {
            base_lines: 5..8,
            side_lines: vec![5..8, 5..8],
            resolved: true,
        }]);
        assert_eq!(find_conflict_at_line(&session, MergeRole::Base, 0), None);
    }

    #[test]
    fn find_conflict_multiple_picks_closest() {
        let session = make_session(vec![
            ConflictRegion {
                base_lines: 10..12,
                side_lines: vec![10..12, 10..12],
                resolved: false,
            },
            ConflictRegion {
                base_lines: 30..32,
                side_lines: vec![30..32, 30..32],
                resolved: false,
            },
            ConflictRegion {
                base_lines: 50..52,
                side_lines: vec![50..52, 50..52],
                resolved: false,
            },
        ]);
        assert_eq!(
            find_conflict_at_line(&session, MergeRole::Base, 28),
            Some(1)
        );
    }
}
