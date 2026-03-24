use super::*;

use helix_view::merge;

fn two_side_conflict_content() -> Vec<u8> {
    b"before\n\
      <<<<<<< Conflict 1 of 1\n\
      %%%%%%% Changes from base to side #1\n\
      -base_line\n\
      +left_line\n\
      +++++++ Contents of side #2\n\
      right_line\n\
      >>>>>>> Conflict 1 of 1 ends\n\
      after\n"
        .to_vec()
}

fn multi_conflict_content() -> Vec<u8> {
    b"header\n\
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
      footer\n"
        .to_vec()
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_parse_conflict_file_from_disk() -> anyhow::Result<()> {
    let content = two_side_conflict_content();
    let file = helpers::temp_file_with_contents(std::str::from_utf8(&content)?)?;
    let file_content = std::fs::read(file.path())?;
    let parsed = merge::parse_conflict_file(&file_content).unwrap();

    assert_eq!(parsed.num_sides, 2);
    assert_eq!(parsed.conflict_regions.len(), 1);
    assert!(parsed.base_text.contains("base_line"));
    assert!(parsed.side_texts[0].contains("left_line"));
    assert!(parsed.side_texts[1].contains("right_line"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_materialize_and_write_to_disk() -> anyhow::Result<()> {
    let content = two_side_conflict_content();
    let parsed = merge::parse_conflict_file(&content).unwrap();

    let side_refs: Vec<&str> = parsed.side_texts.iter().map(|s| s.as_str()).collect();
    let output = merge::materialize_conflicts(
        &parsed.base_text,
        &side_refs,
        &parsed.conflict_regions,
        parsed.conflict_marker_len,
    );

    let dir = tempfile::tempdir()?;
    let out_path = dir.path().join("output.txt");
    std::fs::write(&out_path, &output)?;

    let written = std::fs::read(&out_path)?;
    let reparsed = merge::parse_conflict_file(&written).unwrap();
    assert_eq!(reparsed.num_sides, 2);
    assert_eq!(reparsed.conflict_regions.len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_resolve_and_write_clean() -> anyhow::Result<()> {
    let content = two_side_conflict_content();
    let mut parsed = merge::parse_conflict_file(&content).unwrap();

    for region in &mut parsed.conflict_regions {
        region.resolved = true;
    }

    let side_refs: Vec<&str> = parsed.side_texts.iter().map(|s| s.as_str()).collect();
    let output = merge::materialize_conflicts(
        &parsed.base_text,
        &side_refs,
        &parsed.conflict_regions,
        parsed.conflict_marker_len,
    );

    let dir = tempfile::tempdir()?;
    let out_path = dir.path().join("resolved.txt");
    std::fs::write(&out_path, &output)?;

    let written = std::fs::read(&out_path)?;
    assert!(merge::parse_conflict_file(&written).is_none());

    let text = String::from_utf8(written)?;
    assert!(text.contains("before\n"));
    assert!(text.contains("after\n"));
    assert!(!text.contains("<<<<<<<"));
    assert!(!text.contains(">>>>>>>"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_partial_resolve_preserves_remaining() -> anyhow::Result<()> {
    let content = multi_conflict_content();
    let mut parsed = merge::parse_conflict_file(&content).unwrap();
    assert_eq!(parsed.conflict_regions.len(), 2);

    parsed.conflict_regions[0].resolved = true;

    let side_refs: Vec<&str> = parsed.side_texts.iter().map(|s| s.as_str()).collect();
    let output = merge::materialize_conflicts(
        &parsed.base_text,
        &side_refs,
        &parsed.conflict_regions,
        parsed.conflict_marker_len,
    );

    let reparsed = merge::parse_conflict_file(&output).unwrap();
    assert_eq!(reparsed.conflict_regions.len(), 1);

    let output_str = String::from_utf8(output)?;
    assert!(output_str.contains("header\n"));
    assert!(output_str.contains("middle\n"));
    assert!(output_str.contains("footer\n"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_session_lifecycle() -> anyhow::Result<()> {
    let content = multi_conflict_content();
    let parsed = merge::parse_conflict_file(&content).unwrap();

    assert_eq!(parsed.num_sides, 2);
    assert_eq!(parsed.conflict_regions.len(), 2);

    let mut regions = parsed.conflict_regions;
    assert_eq!(regions.iter().filter(|r| !r.resolved).count(), 2);

    regions[0].resolved = true;
    assert_eq!(regions.iter().filter(|r| !r.resolved).count(), 1);

    regions[1].resolved = true;
    assert_eq!(regions.iter().filter(|r| !r.resolved).count(), 0);

    let side_refs: Vec<&str> = parsed.side_texts.iter().map(|s| s.as_str()).collect();
    let output = merge::materialize_conflicts(
        &parsed.base_text,
        &side_refs,
        &regions,
        parsed.conflict_marker_len,
    );
    let output_str = String::from_utf8(output)?;
    assert!(!output_str.contains("<<<<<<<"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_derive_regions_undo_round_trip() -> anyhow::Result<()> {
    // Simulate accept-then-undo: deriving regions from the original base text
    // (post-undo state) should report the conflict as unresolved again.
    let content = two_side_conflict_content();
    let parsed = merge::parse_conflict_file(&content).unwrap();

    let r0 = &parsed.conflict_regions[0];
    let base_lines: Vec<&str> = parsed.base_text.lines().collect();
    let side0_lines: Vec<&str> = parsed.side_texts[0].lines().collect();
    let s0 = &r0.side_lines[0];

    // Build a "post-accept" base text where region 0 is replaced with side 0 content.
    let mut accepted = String::new();
    for line in &base_lines[..r0.base_lines.start] {
        accepted.push_str(line);
        accepted.push('\n');
    }
    for line in &side0_lines[s0.start..s0.end] {
        accepted.push_str(line);
        accepted.push('\n');
    }
    for line in &base_lines[r0.base_lines.end..] {
        accepted.push_str(line);
        accepted.push('\n');
    }

    let after_accept = merge::derive_regions(
        &accepted,
        &parsed.base_text,
        &parsed.side_texts,
        &parsed.conflict_regions,
    );
    assert!(after_accept[0].resolved, "accept should mark region resolved");

    // Now revert (undo) — derive against the original base text.
    let after_undo = merge::derive_regions(
        &parsed.base_text,
        &parsed.base_text,
        &parsed.side_texts,
        &parsed.conflict_regions,
    );
    assert!(!after_undo[0].resolved, "undo should restore unresolved state");

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_region_base_delta_adjustment() -> anyhow::Result<()> {
    let content = multi_conflict_content();
    let parsed = merge::parse_conflict_file(&content).unwrap();

    let r0 = &parsed.conflict_regions[0];
    let r1 = &parsed.conflict_regions[1];

    let old_base_len = r0.base_lines.end - r0.base_lines.start;
    let side0_len = r0.side_lines[0].end - r0.side_lines[0].start;
    let delta = side0_len as isize - old_base_len as isize;

    let adjusted_r1_start = (r1.base_lines.start as isize + delta) as usize;
    let adjusted_r1_end = (r1.base_lines.end as isize + delta) as usize;

    assert!(adjusted_r1_start <= adjusted_r1_end);

    Ok(())
}
