use std::ops::Range;
use std::path::{Path, PathBuf};

use bstr::ByteSlice;
use jj_lib::config::StackedConfig;
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::conflicts::{self, ConflictMarkerStyle, ConflictMaterializeOptions};
use jj_lib::files::FileMergeHunkLevel;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::NothingMatcher;
use jj_lib::merge::{Merge, SameChange};
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo::StoreFactories;
use jj_lib::settings::UserSettings;
use jj_lib::tree_merge::MergeOptions;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::workspace::{self, Workspace};
use pollster::FutureExt as _;

use crate::{DocumentId, ViewId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeRole {
    Side(usize),
    Base,
}

#[derive(Debug)]
pub struct MergeSide {
    pub doc_id: DocumentId,
    pub view_id: ViewId,
    pub role: MergeRole,
}

#[derive(Debug, Clone)]
pub struct ConflictRegion {
    pub base_lines: Range<usize>,
    pub side_lines: Vec<Range<usize>>,
    pub resolved: bool,
}

pub struct MergeSession {
    pub original_path: PathBuf,
    pub original_doc_id: Option<DocumentId>,
    pub sides: Vec<MergeSide>,
    pub base: MergeSide,
    pub conflict_regions: Vec<ConflictRegion>,
    pub num_sides: usize,
    pub conflict_marker_len: usize,
    pub original_base_text: String,
    pub original_side_texts: Vec<String>,
    pub original_regions: Vec<ConflictRegion>,
    /// Owns the on-disk backing files for each merge pane so language servers
    /// have a real URL to attach to. Dropped on tear-down, which deletes the
    /// directory and everything inside.
    pub _tempdir: Option<tempfile::TempDir>,
}

impl MergeSession {
    pub fn all_sides(&self) -> impl Iterator<Item = &MergeSide> {
        std::iter::once(&self.base).chain(self.sides.iter())
    }

    pub fn all_doc_ids(&self) -> Vec<DocumentId> {
        let mut ids = vec![self.base.doc_id];
        ids.extend(self.sides.iter().map(|s| s.doc_id));
        ids
    }

    pub fn all_view_ids(&self) -> Vec<ViewId> {
        let mut ids = vec![self.base.view_id];
        ids.extend(self.sides.iter().map(|s| s.view_id));
        ids
    }

    pub fn find_role(&self, doc_id: DocumentId) -> Option<MergeRole> {
        if self.base.doc_id == doc_id {
            return Some(MergeRole::Base);
        }
        self.sides
            .iter()
            .find(|s| s.doc_id == doc_id)
            .map(|s| s.role)
    }

    pub fn find_side_by_view(&self, view_id: ViewId) -> Option<&MergeSide> {
        if self.base.view_id == view_id {
            return Some(&self.base);
        }
        self.sides.iter().find(|s| s.view_id == view_id)
    }

    pub fn lines_for_role<'a>(
        &self,
        region: &'a ConflictRegion,
        role: MergeRole,
    ) -> &'a Range<usize> {
        match role {
            MergeRole::Base => &region.base_lines,
            MergeRole::Side(i) => &region.side_lines[i],
        }
    }

    pub fn unresolved_count(&self) -> usize {
        self.conflict_regions
            .iter()
            .filter(|r| !r.resolved)
            .count()
    }

}

pub struct ParsedConflict {
    pub base_text: String,
    pub side_texts: Vec<String>,
    pub conflict_regions: Vec<ConflictRegion>,
    pub num_sides: usize,
    pub conflict_marker_len: usize,
}

fn count_lines(s: &[u8]) -> usize {
    if s.is_empty() {
        return 0;
    }
    s.lines_with_terminator().count()
}

pub fn parse_conflict_file(content: &[u8]) -> Option<ParsedConflict> {
    for num_sides in 2..=8 {
        if let Some(parsed) = try_parse_with_sides(content, num_sides) {
            return Some(parsed);
        }
    }
    None
}

fn try_parse_with_sides(content: &[u8], num_sides: usize) -> Option<ParsedConflict> {
    let marker_len = conflicts::MIN_CONFLICT_MARKER_LEN;
    let hunks = conflicts::parse_conflict(content, num_sides, marker_len)?;

    let mut base_text = Vec::new();
    let mut side_texts: Vec<Vec<u8>> = (0..num_sides).map(|_| Vec::new()).collect();
    let mut conflict_regions = Vec::new();

    let mut base_line = 0usize;
    let mut side_lines: Vec<usize> = vec![0; num_sides];

    for hunk in &hunks {
        if let Some(resolved) = hunk.as_resolved() {
            let bytes: &[u8] = resolved.as_ref();
            let lc = count_lines(bytes);

            base_text.extend_from_slice(bytes);
            for (i, side) in side_texts.iter_mut().enumerate() {
                side.extend_from_slice(bytes);
                side_lines[i] += lc;
            }
            base_line += lc;
        } else {
            let base_bytes: &[u8] = hunk
                .removes()
                .next()
                .map(|b| b.as_ref())
                .unwrap_or(b"");
            let base_lc = count_lines(base_bytes);
            base_text.extend_from_slice(base_bytes);

            let mut region_side_lines = Vec::with_capacity(num_sides);
            for (i, side) in side_texts.iter_mut().enumerate() {
                let side_bytes: &[u8] = hunk
                    .get_add(i)
                    .map(|b| b.as_ref())
                    .unwrap_or(b"");
                let side_lc = count_lines(side_bytes);
                side.extend_from_slice(side_bytes);
                region_side_lines.push(side_lines[i]..side_lines[i] + side_lc);
                side_lines[i] += side_lc;
            }

            conflict_regions.push(ConflictRegion {
                base_lines: base_line..base_line + base_lc,
                side_lines: region_side_lines,
                resolved: false,
            });

            base_line += base_lc;
        }
    }

    if conflict_regions.is_empty() {
        return None;
    }

    Some(ParsedConflict {
        base_text: String::from_utf8_lossy(&base_text).into_owned(),
        side_texts: side_texts
            .into_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect(),
        conflict_regions,
        num_sides,
        conflict_marker_len: marker_len,
    })
}

fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".jj").is_dir() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

fn load_jj_config() -> Option<StackedConfig> {
    use jj_lib::config::{ConfigLayer, ConfigSource};
    use std::process::Command;

    let output = Command::new("jj")
        .args(["config", "list", "--include-defaults"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let toml_text = String::from_utf8_lossy(&output.stdout);
    let layer = ConfigLayer::parse(ConfigSource::Default, &toml_text).ok()?;
    let mut config = StackedConfig::empty();
    config.add_layer(layer);

    // jj-lib pulled into helix is not compiled with the `watchman` feature, so
    // a user setting like `fsmonitor.backend = "watchman"` will fail every
    // snapshot we attempt. Force-disable the fsmonitor for our in-process
    // workspace operations.
    if let Ok(override_layer) =
        ConfigLayer::parse(ConfigSource::CommandArg, "fsmonitor.backend = \"none\"\n")
    {
        config.add_layer(override_layer);
    }

    Some(config)
}

fn load_workspace(cwd: &Path) -> Option<(Workspace, UserSettings, PathBuf)> {
    let ws_root = match find_workspace_root(cwd) {
        Some(r) => r,
        None => {
            log::debug!("merge: no .jj directory found from {}", cwd.display());
            return None;
        }
    };

    let config = match load_jj_config() {
        Some(c) => c,
        None => {
            log::warn!("merge: failed to load jj config");
            return None;
        }
    };
    let settings = match UserSettings::from_config(config) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("merge: failed to create jj settings: {}", e);
            return None;
        }
    };

    let ws = match Workspace::load(
        &settings,
        &ws_root,
        &StoreFactories::default(),
        &workspace::default_working_copy_factories(),
    ) {
        Ok(ws) => ws,
        Err(e) => {
            log::warn!(
                "merge: failed to load jj workspace at {}: {:?}",
                ws_root.display(),
                e
            );
            return None;
        }
    };

    Some((ws, settings, ws_root))
}

/// Snapshot the working copy in-memory and return the resulting tree.
///
/// We deliberately do not call `finish()` on the lock — the snapshot is
/// short-lived state used only for the current query. Persisting would require
/// also rewriting the working-copy commit (jj-cli's full snapshot flow), which
/// we don't need for read-only conflict discovery.
fn snapshot_tree(ws: &mut Workspace) -> Option<MergedTree> {
    let mut locked = match ws.start_working_copy_mutation() {
        Ok(l) => l,
        Err(e) => {
            log::warn!("merge: failed to lock working copy: {:?}", e);
            return None;
        }
    };

    let nothing = NothingMatcher;
    let options = SnapshotOptions {
        base_ignores: GitIgnoreFile::empty(),
        progress: None,
        start_tracking_matcher: &nothing,
        force_tracking_matcher: &nothing,
        max_new_file_size: u64::MAX,
    };

    match locked.locked_wc().snapshot(&options).block_on() {
        Ok((tree, _stats)) => Some(tree),
        Err(e) => {
            log::warn!("merge: snapshot failed: {:?}", e);
            None
        }
    }
}

/// Best-effort: refresh jj's view of the working copy so a subsequent
/// [`find_jj_conflicts`] sees on-disk writes from helix. Currently a no-op
/// because `find_jj_conflicts` does its own snapshot internally; kept as a
/// public hook for callers that want to invalidate cached state explicitly.
pub fn snapshot_jj_working_copy(cwd: &Path) {
    let Some((mut ws, _settings, _root)) = load_workspace(cwd) else {
        return;
    };
    let _ = snapshot_tree(&mut ws);
}

pub fn find_jj_conflicts(cwd: &Path) -> Vec<PathBuf> {
    let (mut ws, _settings, _ws_root) = match load_workspace(cwd) {
        Some(x) => x,
        None => return Vec::new(),
    };

    let tree = match snapshot_tree(&mut ws) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let root = ws.workspace_root();
    tree.conflicts()
        .filter_map(|(repo_path, result)| {
            result.ok()?;
            repo_path.to_fs_path(root).ok()
        })
        .collect()
}

/// Re-derive the live `conflict_regions` for a merge session by comparing the
/// current base document text against the immutable parsed snapshot.
///
/// Used so undo/redo, direct edits, and accept_side all funnel through one source
/// of truth (the document text itself) rather than maintaining mutable region
/// state in parallel.
pub fn derive_regions(
    current_base: &str,
    original_base: &str,
    original_sides: &[String],
    original_regions: &[ConflictRegion],
) -> Vec<ConflictRegion> {
    let current_lines: Vec<&[u8]> = current_base.as_bytes().lines_with_terminator().collect();
    let original_lines: Vec<&[u8]> = original_base.as_bytes().lines_with_terminator().collect();
    let side_line_vecs: Vec<Vec<&[u8]>> = original_sides
        .iter()
        .map(|s| s.as_bytes().lines_with_terminator().collect())
        .collect();

    let mut result = Vec::with_capacity(original_regions.len());
    let mut current_pos = 0usize;
    let mut original_pos = 0usize;

    for (idx, orig_region) in original_regions.iter().enumerate() {
        let prefix_len = orig_region.base_lines.start.saturating_sub(original_pos);
        current_pos = current_pos.saturating_add(prefix_len);

        let orig_len = orig_region.base_lines.end - orig_region.base_lines.start;
        let orig_slice = &original_lines[orig_region.base_lines.start..orig_region.base_lines.end];

        let mut matched: Option<(usize, bool)> = None;

        if current_pos + orig_len <= current_lines.len()
            && current_lines[current_pos..current_pos + orig_len] == *orig_slice
        {
            matched = Some((orig_len, false));
        } else {
            for (i, side_range) in orig_region.side_lines.iter().enumerate() {
                let side_lines = &side_line_vecs[i];
                let side_len = side_range.end - side_range.start;
                if side_range.end > side_lines.len() {
                    continue;
                }
                let side_slice = &side_lines[side_range.start..side_range.end];
                if current_pos + side_len <= current_lines.len()
                    && current_lines[current_pos..current_pos + side_len] == *side_slice
                {
                    matched = Some((side_len, true));
                    break;
                }
            }
        }

        let (length, resolved) = match matched {
            Some(m) => m,
            None => {
                let next_anchor = original_regions
                    .get(idx + 1)
                    .map(|r| r.base_lines.start)
                    .unwrap_or(original_lines.len());
                let anchor_start = orig_region.base_lines.end;
                let anchor_end = (anchor_start + 3).min(next_anchor);
                if anchor_start >= anchor_end {
                    let len = current_lines.len().saturating_sub(current_pos);
                    (len, true)
                } else {
                    let anchor = &original_lines[anchor_start..anchor_end];
                    let mut found_offset = None;
                    let search_end = current_lines.len().saturating_sub(anchor.len());
                    for i in current_pos..=search_end {
                        if current_lines[i..i + anchor.len()] == *anchor {
                            found_offset = Some(i - current_pos);
                            break;
                        }
                    }
                    let len = found_offset.unwrap_or(current_lines.len().saturating_sub(current_pos));
                    (len, true)
                }
            }
        };

        result.push(ConflictRegion {
            base_lines: current_pos..current_pos + length,
            side_lines: orig_region.side_lines.clone(),
            resolved,
        });

        current_pos += length;
        original_pos = orig_region.base_lines.end;
    }

    result
}

pub fn materialize_conflicts(
    base_text: &str,
    side_texts: &[&str],
    regions: &[ConflictRegion],
    marker_len: usize,
) -> Vec<u8> {
    let base_lines: Vec<&[u8]> = base_text.as_bytes().lines_with_terminator().collect();
    let side_line_vecs: Vec<Vec<&[u8]>> = side_texts
        .iter()
        .map(|t| t.as_bytes().lines_with_terminator().collect())
        .collect();

    let mut output = Vec::new();
    let mut base_pos = 0;

    for region in regions {
        if base_pos < region.base_lines.start {
            for line in &base_lines[base_pos..region.base_lines.start] {
                output.extend_from_slice(line);
            }
        }

        if region.resolved {
            for line in &base_lines[region.base_lines.start..region.base_lines.end] {
                output.extend_from_slice(line);
            }
        } else {
            let base_slice: Vec<u8> = base_lines
                [region.base_lines.start..region.base_lines.end]
                .concat();
            let mut side_slices: Vec<Vec<u8>> = Vec::new();
            for (i, sl) in side_line_vecs.iter().enumerate() {
                let r = &region.side_lines[i];
                let slice: Vec<u8> = sl[r.start..r.end].concat();
                side_slices.push(slice);
            }

            // jj_lib::Merge expects an alternating add/remove vector of length
            // 2N-1: [side0, base, side1, base, side2, ..., base, sideN-1].
            // Reusing the same base slice between each pair is an
            // approximation (the original conflict may have had distinct
            // bases per pair) but matches what we surfaced during parsing,
            // where we only kept the first `removes()` entry.
            let mut merge_values: Vec<Vec<u8>> =
                Vec::with_capacity(side_slices.len() * 2 - 1);
            for (i, side) in side_slices.iter().enumerate() {
                if i > 0 {
                    merge_values.push(base_slice.clone());
                }
                merge_values.push(side.clone());
            }
            let merge = Merge::from_vec(merge_values);

            let options = ConflictMaterializeOptions {
                marker_style: ConflictMarkerStyle::Diff,
                marker_len: Some(marker_len),
                merge: MergeOptions {
                    hunk_level: FileMergeHunkLevel::Line,
                    same_change: SameChange::Accept,
                },
            };
            let materialized = conflicts::materialize_merge_result_to_bytes(
                &merge,
                &ConflictLabels::unlabeled(),
                &options,
            );
            output.extend_from_slice(&materialized);
        }

        base_pos = region.base_lines.end;
    }

    if base_pos < base_lines.len() {
        for line in &base_lines[base_pos..] {
            output.extend_from_slice(line);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_lines_empty() {
        assert_eq!(count_lines(b""), 0);
    }

    #[test]
    fn count_lines_single_no_newline() {
        assert_eq!(count_lines(b"hello"), 1);
    }

    #[test]
    fn count_lines_single_with_newline() {
        assert_eq!(count_lines(b"hello\n"), 1);
    }

    #[test]
    fn count_lines_multiple() {
        assert_eq!(count_lines(b"a\nb\nc\n"), 3);
    }

    #[test]
    fn count_lines_trailing_no_newline() {
        assert_eq!(count_lines(b"a\nb"), 2);
    }

    fn make_two_side_conflict() -> &'static [u8] {
        b"before\n\
          <<<<<<< Conflict 1 of 1\n\
          %%%%%%% Changes from base to side #1\n\
          -base\n\
          +left\n\
          +++++++ Contents of side #2\n\
          right\n\
          >>>>>>> Conflict 1 of 1 ends\n\
          after\n"
    }

    #[test]
    fn parse_conflict_file_two_sides() {
        let content = make_two_side_conflict();
        let parsed = parse_conflict_file(content).unwrap();

        assert_eq!(parsed.num_sides, 2);
        assert_eq!(parsed.conflict_regions.len(), 1);
        assert!(!parsed.conflict_regions[0].resolved);
        assert_eq!(parsed.conflict_marker_len, 7);

        assert!(parsed.base_text.contains("before"));
        assert!(parsed.base_text.contains("after"));
        assert!(parsed.base_text.contains("base"));

        assert!(parsed.side_texts[0].contains("left"));
        assert!(parsed.side_texts[1].contains("right"));
    }

    #[test]
    fn parse_conflict_file_no_conflicts() {
        let content = b"just some normal text\nwith multiple lines\n";
        assert!(parse_conflict_file(content).is_none());
    }

    #[test]
    fn parse_conflict_file_empty() {
        assert!(parse_conflict_file(b"").is_none());
    }

    #[test]
    fn parse_conflict_regions_track_lines() {
        let content = make_two_side_conflict();
        let parsed = parse_conflict_file(content).unwrap();

        let region = &parsed.conflict_regions[0];
        assert!(!region.base_lines.is_empty() || region.base_lines.start == region.base_lines.end);
        assert_eq!(region.side_lines.len(), 2);
    }

    #[test]
    fn parse_conflict_multiple_conflicts() {
        let content = b"top\n\
            <<<<<<< Conflict 1 of 2\n\
            %%%%%%% Changes from base to side #1\n\
            -base1\n\
            +left1\n\
            +++++++ Contents of side #2\n\
            right1\n\
            >>>>>>> Conflict 1 of 2 ends\n\
            middle\n\
            <<<<<<< Conflict 2 of 2\n\
            %%%%%%% Changes from base to side #1\n\
            -base2\n\
            +left2\n\
            +++++++ Contents of side #2\n\
            right2\n\
            >>>>>>> Conflict 2 of 2 ends\n\
            bottom\n";

        let parsed = parse_conflict_file(content).unwrap();
        assert_eq!(parsed.num_sides, 2);
        assert_eq!(parsed.conflict_regions.len(), 2);

        assert!(parsed.base_text.contains("base1"));
        assert!(parsed.base_text.contains("base2"));
        assert!(parsed.base_text.contains("middle"));

        assert!(parsed.side_texts[0].contains("left1"));
        assert!(parsed.side_texts[0].contains("left2"));
        assert!(parsed.side_texts[1].contains("right1"));
        assert!(parsed.side_texts[1].contains("right2"));
    }

    fn make_session_with_regions(regions: Vec<ConflictRegion>) -> MergeSession {
        MergeSession {
            original_path: PathBuf::from("/test/file.rs"),
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
            _tempdir: None,
        }
    }

    #[test]
    fn session_unresolved_count_all_unresolved() {
        let session = make_session_with_regions(vec![
            ConflictRegion {
                base_lines: 5..8,
                side_lines: vec![5..8, 5..9],
                resolved: false,
            },
            ConflictRegion {
                base_lines: 12..15,
                side_lines: vec![12..14, 12..16],
                resolved: false,
            },
        ]);
        assert_eq!(session.unresolved_count(), 2);
    }

    #[test]
    fn session_unresolved_count_some_resolved() {
        let session = make_session_with_regions(vec![
            ConflictRegion {
                base_lines: 5..8,
                side_lines: vec![5..8, 5..9],
                resolved: true,
            },
            ConflictRegion {
                base_lines: 12..15,
                side_lines: vec![12..14, 12..16],
                resolved: false,
            },
        ]);
        assert_eq!(session.unresolved_count(), 1);
    }

    #[test]
    fn session_unresolved_count_all_resolved() {
        let session = make_session_with_regions(vec![
            ConflictRegion {
                base_lines: 5..8,
                side_lines: vec![5..8, 5..9],
                resolved: true,
            },
        ]);
        assert_eq!(session.unresolved_count(), 0);
    }

    #[test]
    fn session_find_role_base() {
        let base_id = DocumentId::default();
        let session = MergeSession {
            original_path: PathBuf::from("/test"),
            original_doc_id: None,
            sides: vec![],
            base: MergeSide {
                doc_id: base_id,
                view_id: ViewId::default(),
                role: MergeRole::Base,
            },
            conflict_regions: vec![],
            num_sides: 2,
            conflict_marker_len: 7,
            original_base_text: String::new(),
            original_side_texts: Vec::new(),
            original_regions: Vec::new(),
            _tempdir: None,
        };
        assert_eq!(session.find_role(base_id), Some(MergeRole::Base));
    }

    #[test]
    fn session_find_role_unknown() {
        let session = make_session_with_regions(vec![]);
        let unknown_id = {
            let mut id = DocumentId::default();
            for _ in 0..10 {
                id = DocumentId(std::num::NonZeroUsize::new(id.0.get() + 1).unwrap());
            }
            id
        };
        assert_eq!(session.find_role(unknown_id), None);
    }

    #[test]
    fn session_all_doc_ids() {
        let session = make_session_with_regions(vec![]);
        let ids = session.all_doc_ids();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn session_all_view_ids() {
        let session = make_session_with_regions(vec![]);
        let ids = session.all_view_ids();
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn session_lines_for_role_base() {
        let region = ConflictRegion {
            base_lines: 10..15,
            side_lines: vec![10..13, 10..17],
            resolved: false,
        };
        let session = make_session_with_regions(vec![region.clone()]);
        assert_eq!(session.lines_for_role(&region, MergeRole::Base), &(10..15));
    }

    #[test]
    fn session_lines_for_role_side() {
        let region = ConflictRegion {
            base_lines: 10..15,
            side_lines: vec![10..13, 10..17],
            resolved: false,
        };
        let session = make_session_with_regions(vec![region.clone()]);
        assert_eq!(
            session.lines_for_role(&region, MergeRole::Side(0)),
            &(10..13)
        );
        assert_eq!(
            session.lines_for_role(&region, MergeRole::Side(1)),
            &(10..17)
        );
    }

    #[test]
    fn find_workspace_root_with_jj() {
        let dir = tempfile::tempdir().unwrap();
        let jj_dir = dir.path().join(".jj");
        std::fs::create_dir(&jj_dir).unwrap();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();

        assert_eq!(
            find_workspace_root(&sub),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn find_workspace_root_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_workspace_root(dir.path()), None);
    }

    #[test]
    fn materialize_resolved_region_uses_base() {
        let base = "aaa\nbbb\nccc\n";
        let side1 = "xxx\nbbb\nccc\n";
        let side2 = "aaa\nyyy\nccc\n";
        let regions = vec![ConflictRegion {
            base_lines: 0..1,
            side_lines: vec![0..1, 0..1],
            resolved: true,
        }];

        let output = materialize_conflicts(base, &[side1, side2], &regions, 7);
        let output_str = String::from_utf8(output).unwrap();
        assert!(output_str.starts_with("aaa\n"));
        assert!(output_str.contains("bbb\n"));
        assert!(output_str.contains("ccc\n"));
    }

    #[test]
    fn materialize_preserves_text_around_conflicts() {
        let base = "before\nconflict\nafter\n";
        let side1 = "before\nleft\nafter\n";
        let side2 = "before\nright\nafter\n";
        let regions = vec![ConflictRegion {
            base_lines: 1..2,
            side_lines: vec![1..2, 1..2],
            resolved: false,
        }];

        let output = materialize_conflicts(base, &[side1, side2], &regions, 7);
        let output_str = String::from_utf8(output).unwrap();
        assert!(output_str.starts_with("before\n"));
        assert!(output_str.ends_with("after\n"));
    }

    #[test]
    fn parse_and_rematerialize_roundtrip() {
        let content = make_two_side_conflict();
        let parsed = parse_conflict_file(content).unwrap();

        let side_refs: Vec<&str> = parsed.side_texts.iter().map(|s| s.as_str()).collect();
        let output = materialize_conflicts(
            &parsed.base_text,
            &side_refs,
            &parsed.conflict_regions,
            parsed.conflict_marker_len,
        );

        let reparsed = parse_conflict_file(&output).unwrap();
        assert_eq!(reparsed.num_sides, parsed.num_sides);
        assert_eq!(reparsed.conflict_regions.len(), parsed.conflict_regions.len());
    }

    #[test]
    fn resolve_one_of_two_conflicts_rematerialize() {
        let content = b"top\n\
            <<<<<<< Conflict 1 of 2\n\
            %%%%%%% Changes from base to side #1\n\
            -base1\n\
            +left1\n\
            +++++++ Contents of side #2\n\
            right1\n\
            >>>>>>> Conflict 1 of 2 ends\n\
            middle\n\
            <<<<<<< Conflict 2 of 2\n\
            %%%%%%% Changes from base to side #1\n\
            -base2\n\
            +left2\n\
            +++++++ Contents of side #2\n\
            right2\n\
            >>>>>>> Conflict 2 of 2 ends\n\
            bottom\n";

        let mut parsed = parse_conflict_file(content).unwrap();
        assert_eq!(parsed.conflict_regions.len(), 2);

        parsed.conflict_regions[0].resolved = true;

        let side_refs: Vec<&str> = parsed.side_texts.iter().map(|s| s.as_str()).collect();
        let output = materialize_conflicts(
            &parsed.base_text,
            &side_refs,
            &parsed.conflict_regions,
            parsed.conflict_marker_len,
        );

        let reparsed = parse_conflict_file(&output).unwrap();
        assert_eq!(reparsed.conflict_regions.len(), 1);
    }

    #[test]
    fn resolve_all_conflicts_produces_clean_file() {
        let content = make_two_side_conflict();
        let mut parsed = parse_conflict_file(content).unwrap();

        for region in &mut parsed.conflict_regions {
            region.resolved = true;
        }

        let side_refs: Vec<&str> = parsed.side_texts.iter().map(|s| s.as_str()).collect();
        let output = materialize_conflicts(
            &parsed.base_text,
            &side_refs,
            &parsed.conflict_regions,
            parsed.conflict_marker_len,
        );
        let output_str = String::from_utf8(output).unwrap();

        assert!(!output_str.contains("<<<<<<<"));
        assert!(!output_str.contains(">>>>>>>"));
        assert!(!output_str.contains("%%%%%%%"));
        assert!(!output_str.contains("+++++++"));
    }

    #[test]
    fn parse_conflict_file_to_disk_roundtrip() {
        let content = make_two_side_conflict();
        let parsed = parse_conflict_file(content).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("base.txt");
        let side1_path = dir.path().join("side1.txt");
        let side2_path = dir.path().join("side2.txt");

        std::fs::write(&base_path, &parsed.base_text).unwrap();
        std::fs::write(&side1_path, &parsed.side_texts[0]).unwrap();
        std::fs::write(&side2_path, &parsed.side_texts[1]).unwrap();

        let base_read = std::fs::read_to_string(&base_path).unwrap();
        let side1_read = std::fs::read_to_string(&side1_path).unwrap();
        let side2_read = std::fs::read_to_string(&side2_path).unwrap();

        assert_eq!(base_read, parsed.base_text);
        assert_eq!(side1_read, parsed.side_texts[0]);
        assert_eq!(side2_read, parsed.side_texts[1]);

        let output = materialize_conflicts(
            &base_read,
            &[side1_read.as_str(), side2_read.as_str()],
            &parsed.conflict_regions,
            parsed.conflict_marker_len,
        );

        let reparsed = parse_conflict_file(&output).unwrap();
        assert_eq!(reparsed.num_sides, parsed.num_sides);
        assert_eq!(reparsed.conflict_regions.len(), parsed.conflict_regions.len());
    }

    #[test]
    fn materialize_with_empty_base_region() {
        let base = "before\nafter\n";
        let side1 = "before\nadded_line\nafter\n";
        let side2 = "before\nother_line\nafter\n";
        let regions = vec![ConflictRegion {
            base_lines: 1..1,
            side_lines: vec![1..2, 1..2],
            resolved: false,
        }];

        let output = materialize_conflicts(base, &[side1, side2], &regions, 7);
        let output_str = String::from_utf8(output).unwrap();
        assert!(output_str.contains("before\n"));
        assert!(output_str.contains("after\n"));
    }

    #[test]
    fn session_find_side_by_view() {
        let base_view = ViewId::default();
        let session = MergeSession {
            original_path: PathBuf::from("/test"),
            original_doc_id: None,
            sides: vec![
                MergeSide {
                    doc_id: DocumentId::default(),
                    view_id: ViewId::default(),
                    role: MergeRole::Side(0),
                },
            ],
            base: MergeSide {
                doc_id: DocumentId::default(),
                view_id: base_view,
                role: MergeRole::Base,
            },
            conflict_regions: vec![],
            num_sides: 2,
            conflict_marker_len: 7,
            original_base_text: String::new(),
            original_side_texts: Vec::new(),
            original_regions: Vec::new(),
            _tempdir: None,
        };

        let found = session.find_side_by_view(base_view).unwrap();
        assert_eq!(found.role, MergeRole::Base);
    }

    #[test]
    fn merge_role_equality() {
        assert_eq!(MergeRole::Base, MergeRole::Base);
        assert_eq!(MergeRole::Side(0), MergeRole::Side(0));
        assert_ne!(MergeRole::Side(0), MergeRole::Side(1));
        assert_ne!(MergeRole::Base, MergeRole::Side(0));
    }

    #[test]
    fn session_all_sides_iterator() {
        let session = make_session_with_regions(vec![]);
        let all: Vec<&MergeSide> = session.all_sides().collect();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].role, MergeRole::Base);
        assert_eq!(all[1].role, MergeRole::Side(0));
        assert_eq!(all[2].role, MergeRole::Side(1));
    }

    #[test]
    fn parse_multiline_conflict_hunks() {
        let content = b"header\n\
            <<<<<<< Conflict 1 of 1\n\
            %%%%%%% Changes from base to side #1\n\
            -old_line_1\n\
            -old_line_2\n\
            +new_line_1\n\
            +new_line_2\n\
            +new_line_3\n\
            +++++++ Contents of side #2\n\
            alt_line_1\n\
            >>>>>>> Conflict 1 of 1 ends\n\
            footer\n";

        let parsed = parse_conflict_file(content).unwrap();
        assert_eq!(parsed.conflict_regions.len(), 1);

        let base_lines: Vec<&str> = parsed.base_text.lines().collect();
        assert!(base_lines.contains(&"old_line_1"));
        assert!(base_lines.contains(&"old_line_2"));

        let side1_lines: Vec<&str> = parsed.side_texts[0].lines().collect();
        assert!(side1_lines.contains(&"new_line_1"));
        assert!(side1_lines.contains(&"new_line_2"));
        assert!(side1_lines.contains(&"new_line_3"));

        let side2_lines: Vec<&str> = parsed.side_texts[1].lines().collect();
        assert!(side2_lines.contains(&"alt_line_1"));
    }

    #[test]
    fn derive_regions_unresolved_when_text_unchanged() {
        let content = make_two_side_conflict();
        let parsed = parse_conflict_file(content).unwrap();

        let derived = derive_regions(
            &parsed.base_text,
            &parsed.base_text,
            &parsed.side_texts,
            &parsed.conflict_regions,
        );

        assert_eq!(derived.len(), 1);
        assert!(!derived[0].resolved);
        assert_eq!(derived[0].base_lines, parsed.conflict_regions[0].base_lines);
    }

    #[test]
    fn derive_regions_resolved_when_matches_side() {
        let content = b"top\n\
            <<<<<<< Conflict 1 of 2\n\
            %%%%%%% Changes from base to side #1\n\
            -base1\n\
            +left1\n\
            +++++++ Contents of side #2\n\
            right1\n\
            >>>>>>> Conflict 1 of 2 ends\n\
            middle\n\
            <<<<<<< Conflict 2 of 2\n\
            %%%%%%% Changes from base to side #1\n\
            -base2\n\
            +left2\n\
            +++++++ Contents of side #2\n\
            right2\n\
            >>>>>>> Conflict 2 of 2 ends\n\
            bottom\n";
        let parsed = parse_conflict_file(content).unwrap();
        assert_eq!(parsed.conflict_regions.len(), 2);

        // Replace region 0's base content with side 0 ("left1")
        let r0 = &parsed.conflict_regions[0];
        let base_lines: Vec<&str> = parsed.base_text.lines().collect();
        let side0_lines: Vec<&str> = parsed.side_texts[0].lines().collect();
        let s0 = &r0.side_lines[0];

        let mut current = String::new();
        for line in &base_lines[..r0.base_lines.start] {
            current.push_str(line);
            current.push('\n');
        }
        for line in &side0_lines[s0.start..s0.end] {
            current.push_str(line);
            current.push('\n');
        }
        for line in &base_lines[r0.base_lines.end..] {
            current.push_str(line);
            current.push('\n');
        }

        let derived = derive_regions(
            &current,
            &parsed.base_text,
            &parsed.side_texts,
            &parsed.conflict_regions,
        );

        assert_eq!(derived.len(), 2);
        assert!(derived[0].resolved, "region 0 should be resolved");
        assert!(!derived[1].resolved, "region 1 should still be unresolved");
    }

    #[test]
    fn derive_regions_undo_round_trip() {
        let content = make_two_side_conflict();
        let parsed = parse_conflict_file(content).unwrap();

        // Resolve, then revert to original — derived regions should be unresolved again
        let r0 = &parsed.conflict_regions[0];
        let base_lines: Vec<&str> = parsed.base_text.lines().collect();
        let side0_lines: Vec<&str> = parsed.side_texts[0].lines().collect();
        let s0 = &r0.side_lines[0];

        let mut resolved_text = String::new();
        for line in &base_lines[..r0.base_lines.start] {
            resolved_text.push_str(line);
            resolved_text.push('\n');
        }
        for line in &side0_lines[s0.start..s0.end] {
            resolved_text.push_str(line);
            resolved_text.push('\n');
        }
        for line in &base_lines[r0.base_lines.end..] {
            resolved_text.push_str(line);
            resolved_text.push('\n');
        }

        let after_accept = derive_regions(
            &resolved_text,
            &parsed.base_text,
            &parsed.side_texts,
            &parsed.conflict_regions,
        );
        assert!(after_accept[0].resolved);

        // Now revert to original (simulating undo)
        let after_undo = derive_regions(
            &parsed.base_text,
            &parsed.base_text,
            &parsed.side_texts,
            &parsed.conflict_regions,
        );
        assert!(!after_undo[0].resolved);
    }

    #[test]
    fn derive_regions_custom_resolution_treated_as_resolved() {
        let content = make_two_side_conflict();
        let parsed = parse_conflict_file(content).unwrap();

        let r0 = &parsed.conflict_regions[0];
        let base_lines: Vec<&str> = parsed.base_text.lines().collect();

        let mut current = String::new();
        for line in &base_lines[..r0.base_lines.start] {
            current.push_str(line);
            current.push('\n');
        }
        current.push_str("totally custom user text\n");
        for line in &base_lines[r0.base_lines.end..] {
            current.push_str(line);
            current.push('\n');
        }

        let derived = derive_regions(
            &current,
            &parsed.base_text,
            &parsed.side_texts,
            &parsed.conflict_regions,
        );

        assert_eq!(derived.len(), 1);
        assert!(derived[0].resolved, "custom resolution counts as resolved");
    }

    #[test]
    fn parse_base_line_tracking_with_multiple_conflicts() {
        let content = b"line0\n\
            <<<<<<< Conflict 1 of 2\n\
            %%%%%%% Changes from base to side #1\n\
            -base_a\n\
            +left_a\n\
            +++++++ Contents of side #2\n\
            right_a\n\
            >>>>>>> Conflict 1 of 2 ends\n\
            between1\n\
            between2\n\
            <<<<<<< Conflict 2 of 2\n\
            %%%%%%% Changes from base to side #1\n\
            -base_b\n\
            +left_b\n\
            +++++++ Contents of side #2\n\
            right_b\n\
            >>>>>>> Conflict 2 of 2 ends\n\
            last\n";

        let parsed = parse_conflict_file(content).unwrap();
        assert_eq!(parsed.conflict_regions.len(), 2);

        let r0 = &parsed.conflict_regions[0];
        let r1 = &parsed.conflict_regions[1];
        assert!(r0.base_lines.end <= r1.base_lines.start);
    }
}
