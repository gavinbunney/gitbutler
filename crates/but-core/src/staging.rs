//! Git's staging area, shared with command-line Git and other clients.

use std::{io::BufWriter, path::Component};

use anyhow::{Context as _, ensure};
use bstr::{BStr, ByteSlice};
use gix::index::entry::{Flags, Stage};

use crate::{
    DiffSpec, HunkHeader, IgnoredWorktreeChange, TreeChange, TreeStatus, UnifiedPatch,
    index::{Edit, Stat},
};

/// The two independent diffs of Git's staging area.
#[derive(Debug)]
pub struct Status {
    /// Changes from HEAD to the index, including saved blob IDs.
    pub staged: Vec<TreeChange>,
    /// Changes from the index to the files on disk.
    pub unstaged: Vec<TreeChange>,
    /// Files that cannot currently be committed, including index conflicts.
    pub ignored: Vec<IgnoredWorktreeChange>,
}

/// Write the unconflicted index as a tree, refusing unresolved conflicts.
pub fn tree(repo: &gix::Repository) -> anyhow::Result<gix::ObjectId> {
    let index = repo.index_or_empty()?;
    ensure!(
        index
            .entries()
            .iter()
            .all(|entry| entry.flags.stage() == Stage::Unconflicted),
        "Resolve the index conflicts before committing"
    );
    unconflicted_tree(repo, &index)
}

fn unconflicted_tree(
    repo: &gix::Repository,
    index: &gix::index::State,
) -> anyhow::Result<gix::ObjectId> {
    let mut tree = repo.empty_tree().edit()?;

    for entry in index.entries() {
        if entry.flags.stage() != Stage::Unconflicted || entry.flags.contains(Flags::INTENT_TO_ADD)
        {
            continue;
        }

        let mode = entry
            .mode
            .to_tree_entry_mode()
            .context("Unsupported index entry mode")?;
        tree.upsert(entry.path(index), mode.kind(), entry.id)?;
    }

    Ok(tree.write()?.detach())
}

/// Stage files or selected hunks and lines without changing files on disk or unrelated index entries.
pub fn stage(
    repo: &gix::Repository,
    changes: Vec<DiffSpec>,
    context_lines: u32,
) -> anyhow::Result<()> {
    validate_changes(&changes)?;
    let mut path_check = gix::status::plumbing::SymlinkCheck::new(
        repo.workdir()
            .context("Staging requires a worktree")?
            .to_owned(),
    );
    for change in &changes {
        path_check.verified_path_allow_nonexisting(change.path.as_bstr())?;
    }
    let lock = lock_index(repo)?;
    let mut index = repo.index_or_empty()?.into_owned_or_cloned();
    let (whole_files, selections): (Vec<_>, Vec<_>) = changes
        .into_iter()
        .partition(|change| change.hunk_headers.is_empty());
    let unstaged = if selections.is_empty() {
        Vec::new()
    } else {
        crate::diff::staging_changes(repo)?.unstaged
    };
    let mut edit = Edit::new(&mut index);

    if !whole_files.is_empty() {
        let base = unconflicted_tree(repo, edit.state())?;
        let mut specs: Vec<_> = whole_files.into_iter().map(Ok).collect();
        let (staged, _) =
            crate::tree::apply_worktree_changes(base, repo, &mut specs, context_lines)?;
        let staged = repo.find_tree(staged)?;

        for spec in specs {
            let DiffSpec {
                path,
                previous_path,
                hunk_headers: _,
            } = spec.map_err(|(reason, spec)| {
                anyhow::anyhow!("Could not stage '{}': {reason:?}", spec.path)
            })?;
            if let Some(previous_path) = previous_path {
                edit.remove(previous_path.as_bstr());
            }

            copy_entry(&mut edit, &staged, path.as_bstr())?;
        }
    }

    for selection in selections {
        let change = find_change(&unstaged, &selection)?;
        apply_selection(
            repo,
            &mut edit,
            change,
            &selection.hunk_headers,
            context_lines,
            Direction::Stage,
        )?;
    }

    if edit.finish() {
        write_index(lock, &index)?;
    }

    Ok(())
}

/// Restore files or selected index hunks and lines from HEAD, keeping the worktree untouched.
pub fn unstage(
    repo: &gix::Repository,
    changes: Vec<DiffSpec>,
    context_lines: u32,
) -> anyhow::Result<()> {
    validate_changes(&changes)?;
    let lock = lock_index(repo)?;
    let mut index = repo.index_or_empty()?.into_owned_or_cloned();
    let head = repo.head_tree_id_or_empty()?.object()?.peel_to_tree()?;
    let staged = if changes.iter().any(|change| !change.hunk_headers.is_empty()) {
        crate::diff::staging_changes(repo)?.staged
    } else {
        Vec::new()
    };
    let mut edit = Edit::new(&mut index);

    for selection in changes {
        if !selection.hunk_headers.is_empty() {
            let change = find_change(&staged, &selection)?;
            apply_selection(
                repo,
                &mut edit,
                change,
                &selection.hunk_headers,
                context_lines,
                Direction::Unstage,
            )?;
            continue;
        }

        let DiffSpec {
            path,
            previous_path,
            hunk_headers: _,
        } = selection;
        copy_entry(&mut edit, &head, path.as_bstr())?;

        if let Some(previous_path) = previous_path {
            copy_entry(&mut edit, &head, previous_path.as_bstr())?;
        }
    }

    if edit.finish() {
        write_index(lock, &index)?;
    }

    Ok(())
}

/// Discard selected unstaged hunks or lines relative to the saved index.
///
/// The index is locked but never written. Renames, mode changes, converted diffs and
/// non-regular files are refused, as for partial staging. The caller must validate
/// the displayed selection and record undo before calling this destructive operation.
pub fn discard(
    repo: &gix::Repository,
    selection: &DiffSpec,
    expected_index_tree: gix::ObjectId,
    context_lines: u32,
) -> anyhow::Result<()> {
    validate_changes(std::slice::from_ref(selection))?;
    ensure!(
        !selection.hunk_headers.is_empty(),
        "Select hunks or lines to discard"
    );
    let _lock = lock_index(repo)?;
    ensure!(
        tree(repo)? == expected_index_tree,
        "The staging area changed. Review the files and try again"
    );
    let unstaged = crate::diff::staging_changes(repo)?.unstaged;
    let change = find_change(&unstaged, selection)?;
    // reverse the index-to-worktree patch into a temporary index, never the saved one
    let mut index = gix::index::State::new(repo.object_hash());
    let mut edit = Edit::new(&mut index);
    apply_selection(
        repo,
        &mut edit,
        change,
        &selection.hunk_headers,
        context_lines,
        Direction::Unstage,
    )?;
    edit.finish();

    let root = repo.workdir().context("Discarding requires a worktree")?;
    let mut path_check = gix::status::plumbing::SymlinkCheck::new(root.to_owned());
    let path = path_check.verified_path_allow_nonexisting(selection.path.as_bstr())?;
    let Some(entry) = index.entry_by_path(selection.path.as_bstr()) else {
        std::fs::remove_file(path)?;
        return Ok(());
    };
    let blob = repo.find_blob(entry.id)?;
    let (mut pipeline, _) = repo.filter_pipeline(Some(repo.empty_tree().id))?;
    let mut stream = pipeline.convert_to_worktree(
        &blob.data,
        selection.path.as_bstr(),
        gix::filter::plumbing::pipeline::convert::to_worktree::Options {
            can_delay: gix::filter::plumbing::driver::apply::Delay::Forbid,
            ..Default::default()
        },
    )?;
    let mut builder = tempfile::Builder::new();
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "The file to discard is no longer a regular file"
            );
            builder.permissions(metadata.permissions());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                builder.permissions(std::fs::Permissions::from_mode(
                    if entry.mode == gix::index::entry::Mode::FILE_EXECUTABLE {
                        0o755
                    } else {
                        0o644
                    },
                ));
            }
        }
        Err(err) => return Err(err.into()),
    }
    let mut file = builder.tempfile_in(root)?;
    std::io::copy(&mut stream, &mut file)?;
    std::fs::create_dir_all(path.parent().context("Expected a repository file path")?)?;
    file.persist(path)?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    Stage,
    Unstage,
}

fn find_change<'a>(
    changes: &'a [TreeChange],
    selection: &DiffSpec,
) -> anyhow::Result<&'a TreeChange> {
    changes
        .iter()
        .find(|change| {
            change.path == selection.path
                && change.previous_path()
                    == selection.previous_path.as_ref().map(|path| path.as_bstr())
        })
        .with_context(|| {
            format!(
                "The changes in '{}' are no longer available. Review the diff again",
                selection.path
            )
        })
}

fn apply_selection(
    repo: &gix::Repository,
    edit: &mut Edit<'_>,
    change: &TreeChange,
    selected: &[HunkHeader],
    context_lines: u32,
    direction: Direction,
) -> anyhow::Result<()> {
    use gix::{diff::blob::platform::resource::Data, object::tree::EntryKind};

    let (previous, current) = match change.status {
        TreeStatus::Addition {
            state,
            is_untracked: _,
        } => (None, Some(state)),
        TreeStatus::Deletion { previous_state } => (Some(previous_state), None),
        TreeStatus::Modification {
            previous_state,
            state,
            flags: None,
        } => (Some(previous_state), Some(state)),
        TreeStatus::Modification { flags: Some(_), .. } | TreeStatus::Rename { .. } => {
            anyhow::bail!(
                "Stage or unstage '{}' as a whole file to include its rename or mode change",
                change.path
            );
        }
    };
    ensure!(
        previous
            .iter()
            .chain(current.iter())
            .all(|state| matches!(state.kind, EntryKind::Blob | EntryKind::BlobExecutable)),
        "Partial staging requires a regular text file: '{}'",
        change.path
    );
    let mut filter =
        crate::unified_diff::filter_from_state(repo, current, UnifiedPatch::CONVERSION_MODE)?;
    let headers = |patch| -> anyhow::Result<Vec<HunkHeader>> {
        match patch {
            Some(UnifiedPatch::Patch {
                hunks,
                is_result_of_binary_to_text_conversion: false,
                ..
            }) => Ok(hunks.into_iter().map(Into::into).collect()),
            _ => anyhow::bail!(
                "Partial staging requires a text diff without text conversion: '{}'",
                change.path
            ),
        }
    };
    let hunks = headers(change.unified_patch_with_filter(repo, context_lines, &mut filter)?)?;
    let no_context = selected
        .iter()
        .any(|h| h.old_range().is_null() || h.new_range().is_null())
        .then(|| {
            change
                .unified_patch_with_filter(repo, 0, &mut filter)
                .and_then(headers)
        })
        .transpose()?;
    let (hunks, rejected) = crate::tree::to_additive_hunks(
        selected.iter().copied(),
        &hunks,
        no_context.as_deref().unwrap_or(&hunks),
    )?;
    ensure!(
        rejected.is_empty() && !hunks.is_empty(),
        "The selected lines in '{}' no longer match. Review the diff again",
        change.path
    );
    let (old, new) = filter
        .resources()
        .expect("computing a text patch loads both diff resources");
    let bytes = |data| match data {
        Data::Buffer {
            buf,
            is_derived: false,
        } => Ok(buf),
        Data::Missing => Ok(&[][..]),
        Data::Buffer {
            is_derived: true, ..
        }
        | Data::Binary { .. } => anyhow::bail!("Partial staging requires original text content"),
    };
    let old = bytes(old.data)?;
    let new = bytes(new.data)?;
    let (target, result) = match direction {
        Direction::Stage => (
            current,
            crate::apply_hunks(old.as_bstr(), new.as_bstr(), &hunks)?,
        ),
        Direction::Unstage => {
            let reversed: Vec<_> = hunks.iter().map(HunkHeader::reverse).collect();
            (
                previous,
                crate::apply_hunks(new.as_bstr(), old.as_bstr(), &reversed)?,
            )
        }
    };
    if target.is_none() && result.is_empty() {
        edit.remove(change.path.as_bstr());
    } else {
        let kind = target
            .or(current)
            .or(previous)
            .expect("a change has at least one file state")
            .kind;
        edit.upsert(
            change.path.as_bstr(),
            repo.write_blob(result.as_slice())?.detach(),
            kind.into(),
            Stat::Unknown,
        );
    }
    Ok(())
}

fn copy_entry(edit: &mut Edit<'_>, tree: &gix::Tree<'_>, path: &BStr) -> anyhow::Result<()> {
    match tree.lookup_entry_by_path(gix::path::from_bstr(path)?)? {
        Some(entry) => edit.upsert(path, entry.object_id(), entry.mode().into(), Stat::Unknown),
        None => edit.remove(path),
    }
    Ok(())
}

fn validate_changes(changes: &[DiffSpec]) -> anyhow::Result<()> {
    for DiffSpec {
        path,
        previous_path,
        hunk_headers,
    } in changes
    {
        for hunk in hunk_headers {
            ensure!(
                hunk.old_start.checked_add(hunk.old_lines).is_some()
                    && hunk.new_start.checked_add(hunk.new_lines).is_some()
                    && (hunk.old_start > 0 || hunk.old_lines == 0)
                    && (hunk.new_start > 0 || hunk.new_lines == 0),
                "Invalid line selection for '{path}': {hunk:?}"
            );
        }

        for path in std::iter::once(path).chain(previous_path.iter()) {
            let path_on_disk = gix::path::from_bstr(path.as_bstr())?;
            ensure!(
                !path.is_empty()
                    && path_on_disk
                        .components()
                        .all(|part| matches!(part, Component::Normal(_))),
                "Expected a repository-relative file path, got '{path}'"
            );
            for component in path.split_str("/") {
                gix::validate::path::component(
                    component.as_bstr(),
                    None,
                    gix::validate::path::component::Options {
                        protect_windows: cfg!(windows),
                        ..Default::default()
                    },
                )?;
            }
        }
    }
    Ok(())
}

fn lock_index(repo: &gix::Repository) -> anyhow::Result<gix::lock::File> {
    gix::lock::File::acquire_to_update_resource(
        repo.index_path(),
        gix::lock::acquire::Fail::Immediately,
        None,
        0,
    )
    .context("Repository index is locked by another process")
}

fn write_index(lock: gix::lock::File, index: &gix::index::File) -> anyhow::Result<()> {
    let mut out = BufWriter::new(lock);
    index.write_to(&mut out, Default::default())?;
    out.into_inner().map_err(|err| err.into_error())?.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use but_testsupport::{git, invoke_bash, writable_scenario};

    fn spec(path: &str) -> DiffSpec {
        DiffSpec {
            path: path.into(),
            previous_path: None,
            hunk_headers: Vec::new(),
        }
    }

    fn saved(repo: &gix::Repository, path: &str) -> anyhow::Result<Vec<u8>> {
        let output = git(repo).args(["show", &format!(":{path}")]).output()?;
        ensure!(
            output.status.success(),
            "git show failed: {:?}",
            output.stderr
        );
        Ok(output.stdout)
    }

    fn selection(path: &str, hunk_headers: Vec<crate::HunkHeader>) -> DiffSpec {
        DiffSpec {
            hunk_headers,
            ..spec(path)
        }
    }

    fn hunk(repo: &gix::Repository, change: &TreeChange) -> anyhow::Result<crate::HunkHeader> {
        let Some(crate::UnifiedPatch::Patch { hunks, .. }) = change.unified_patch(repo, 3)? else {
            anyhow::bail!("expected a text patch");
        };
        Ok(hunks.first().expect("the file has changed").into())
    }

    #[test]
    fn split_staging_discard_preserves_filters_file_presence_and_index() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let root = repo.workdir().unwrap();
        std::fs::write(root.join(".gitattributes"), "*.txt text eol=crlf\n")?;
        std::fs::write(root.join("file.txt"), "saved\r\n")?;
        stage(&repo, vec![spec(".gitattributes"), spec("file.txt")], 3)?;
        let index = std::fs::read(repo.index_path())?;
        let tree = tree(&repo)?;
        std::fs::write(root.join("file.txt"), "saved\r\ndiscard\r\nkeep")?;
        discard(
            &repo,
            &selection(
                "file.txt",
                vec![HunkHeader {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 2,
                    new_lines: 1,
                }],
            ),
            tree,
            3,
        )?;
        assert_eq!(
            std::fs::read(root.join("file.txt"))?,
            b"saved\r\nkeep",
            "discard restores through Git's filters and keeps a missing final newline"
        );

        std::fs::remove_file(root.join("file.txt"))?;
        let status = crate::diff::staging_changes(&repo)?;
        let deleted = status
            .unstaged
            .iter()
            .find(|change| change.path == "file.txt")
            .unwrap();
        discard(
            &repo,
            &selection("file.txt", vec![hunk(&repo, deleted)?]),
            tree,
            3,
        )?;
        assert_eq!(
            std::fs::read(root.join("file.txt"))?,
            b"saved\r\n",
            "discard restores a deleted file from its staged version"
        );

        std::fs::write(root.join("new"), "first\nsecond")?;
        discard(
            &repo,
            &selection(
                "new",
                vec![HunkHeader {
                    old_start: 0,
                    old_lines: 0,
                    new_start: 1,
                    new_lines: 1,
                }],
            ),
            tree,
            3,
        )?;
        assert_eq!(
            std::fs::read(root.join("new"))?,
            b"second",
            "partial discard keeps unselected new-file lines"
        );
        let status = crate::diff::staging_changes(&repo)?;
        let added = status
            .unstaged
            .iter()
            .find(|change| change.path == "new")
            .unwrap();
        discard(&repo, &selection("new", vec![hunk(&repo, added)?]), tree, 3)?;
        assert!(
            !root.join("new").exists(),
            "discarding the last new-file line removes the file"
        );
        assert_eq!(
            std::fs::read(repo.index_path())?,
            index,
            "discard never writes the saved index"
        );
        Ok(())
    }

    #[test]
    fn split_staging_partial_lines_preserve_other_changes_and_later_edits() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let path = repo.workdir().unwrap().join("base");
        std::fs::write(&path, "one\ntwo\nthree\nfour\n")?;
        invoke_bash("git add base && git commit -m lines", &repo);
        std::fs::write(&path, "ONE\nTWO\nthree\nfour\n")?;
        let headers = vec![
            crate::HunkHeader {
                old_start: 1,
                old_lines: 1,
                new_start: 0,
                new_lines: 0,
            },
            crate::HunkHeader {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
            },
        ];
        stage(&repo, vec![selection("base", headers.clone())], 3)?;
        assert_eq!(
            saved(&repo, "base")?,
            b"ONE\ntwo\nthree\nfour\n",
            "only the selected replacement is staged"
        );
        stage(&repo, vec![spec("base")], 3)?;
        std::fs::write(&path, "later\nTWO\nthree\nfour\n")?;
        unstage(&repo, vec![selection("base", headers)], 3)?;
        assert_eq!(
            saved(&repo, "base")?,
            b"one\nTWO\nthree\nfour\n",
            "unstaging restores only the selected index lines"
        );
        assert_eq!(
            std::fs::read(&path)?,
            b"later\nTWO\nthree\nfour\n",
            "unstaging never reads later edits into the index or rewrites disk"
        );
        Ok(())
    }

    #[test]
    fn split_staging_partial_hunks_use_index_as_the_base() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let path = repo.workdir().unwrap().join("base");
        let original = (1..=30).map(|n| format!("line {n}\n")).collect::<String>();
        std::fs::write(&path, &original)?;
        invoke_bash("git add base && git commit -m lines", &repo);
        let saved_content = original.replace("line 2\n", "saved 2\n");
        std::fs::write(&path, &saved_content)?;
        stage(&repo, vec![spec("base")], 3)?;
        let later = original.replace("line 29\n", "later 29\n");
        std::fs::write(&path, &later)?;
        let status = crate::diff::staging_changes(&repo)?;
        let Some(crate::UnifiedPatch::Patch { hunks, .. }) =
            status.unstaged[0].unified_patch(&repo, 3)?
        else {
            anyhow::bail!("expected a text patch");
        };
        let last = hunks.last().expect("the last line changed").into();
        stage(&repo, vec![selection("base", vec![last])], 3)?;
        assert_eq!(
            saved(&repo, "base")?,
            saved_content.replace("line 29\n", "later 29\n").as_bytes(),
            "staging a hunk retains a saved change that disk reverted to HEAD"
        );
        let status = crate::diff::staging_changes(&repo)?;
        let first = hunk(&repo, &status.staged[0])?;
        unstage(&repo, vec![selection("base", vec![first])], 3)?;
        assert_eq!(
            saved(&repo, "base")?,
            later.as_bytes(),
            "unstaging one hunk retains the other staged hunk"
        );
        Ok(())
    }

    #[test]
    fn split_staging_partial_addition_and_deletion_preserve_file_presence() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let root = repo.workdir().unwrap();
        let added_line = crate::HunkHeader {
            old_start: 0,
            old_lines: 0,
            new_start: 2,
            new_lines: 1,
        };
        std::fs::write(root.join("new"), "first\nsecond")?;
        stage(&repo, vec![selection("new", vec![added_line])], 3)?;
        assert_eq!(
            saved(&repo, "new")?,
            b"second",
            "partial additions preserve the missing final newline"
        );
        unstage(
            &repo,
            vec![selection(
                "new",
                vec![crate::HunkHeader {
                    new_start: 1,
                    ..added_line
                }],
            )],
            3,
        )?;
        assert!(
            repo.index_or_empty()?.entry_by_path("new".into()).is_none(),
            "unstaging the last added line removes the index entry"
        );
        std::fs::write(root.join("base"), "first\r\nsecond\r\n")?;
        invoke_bash("git add base && git commit -m crlf", &repo);
        std::fs::remove_file(root.join("base"))?;
        let removed_line = crate::HunkHeader {
            old_start: 1,
            old_lines: 1,
            new_start: 0,
            new_lines: 0,
        };
        stage(&repo, vec![selection("base", vec![removed_line])], 3)?;
        assert_eq!(
            saved(&repo, "base")?,
            b"second\r\n",
            "partial deletion preserves remaining bytes and CRLF"
        );
        let status = crate::diff::staging_changes(&repo)?;
        stage(
            &repo,
            vec![selection(
                "base",
                vec![hunk(
                    &repo,
                    status.unstaged.iter().find(|c| c.path == "base").unwrap(),
                )?],
            )],
            3,
        )?;
        assert!(
            repo.index_or_empty()?
                .entry_by_path("base".into())
                .is_none(),
            "staging the complete deletion removes the index entry"
        );
        assert!(
            !root.join("base").exists(),
            "partial operations leave the deleted worktree file alone"
        );
        Ok(())
    }

    #[test]
    fn split_staging_partial_rejection_leaves_the_entire_index_unchanged() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let root = repo.workdir().unwrap();
        std::fs::write(root.join("base"), "changed\n")?;
        std::fs::write(root.join("new"), "new\n")?;
        let before = std::fs::read(repo.index_path())?;
        let invalid = HunkHeader {
            old_start: 0,
            old_lines: 0,
            new_start: 999,
            new_lines: 1,
        };
        assert!(
            stage(
                &repo,
                vec![spec("new"), selection("base", vec![invalid])],
                3
            )
            .is_err(),
            "a stale line selection rejects the batch"
        );
        assert_eq!(
            std::fs::read(repo.index_path())?,
            before,
            "an earlier whole-file edit is not written when a later selection fails"
        );
        std::fs::write(root.join("base"), b"binary\0content")?;
        assert!(
            stage(
                &repo,
                vec![selection(
                    "base",
                    vec![HunkHeader {
                        new_start: 1,
                        ..invalid
                    }]
                )],
                3
            )
            .is_err(),
            "binary files require whole-file staging"
        );
        assert_eq!(
            std::fs::read(repo.index_path())?,
            before,
            "rejected binary selections preserve the index"
        );
        Ok(())
    }

    #[test]
    fn split_staging_saves_content_before_later_edits() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let path = repo.workdir().unwrap().join("base");
        std::fs::write(&path, "saved\n")?;
        stage(&repo, vec![spec("base")], 3)?;
        std::fs::write(&path, "later\n")?;
        assert_eq!(
            saved(&repo, "base")?,
            b"saved\n",
            "the Git index keeps the staged version"
        );
        let status = crate::diff::staging_changes(&repo)?;
        assert_eq!(status.staged.len(), 1, "HEAD differs from the saved index");
        assert_eq!(
            status.unstaged.len(),
            1,
            "later edits differ from the index"
        );
        assert_eq!(
            status.staged[0].path, status.unstaged[0].path,
            "a path can appear in both lists"
        );
        assert_eq!(
            status.staged[0].status.state().map(|state| state.id),
            status.unstaged[0]
                .status
                .previous_state_and_path()
                .map(|(state, _)| state.id),
            "both diffs meet at the same saved blob"
        );
        std::fs::write(&path, "base\n")?;
        let status = crate::diff::staging_changes(&repo)?;
        assert_eq!(
            (status.staged.len(), status.unstaged.len()),
            (1, 1),
            "returning the worktree to HEAD must not hide either diff"
        );
        unstage(&repo, vec![spec("base")], 3)?;
        assert_eq!(
            std::fs::read(&path)?,
            b"base\n",
            "unstaging leaves the worktree untouched"
        );
        Ok(())
    }

    #[test]
    fn split_staging_additions_deletions_and_unrelated_entries() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let root = repo.workdir().unwrap();
        std::fs::write(root.join("new file"), "new\n")?;
        std::fs::remove_file(root.join("base"))?;
        stage(&repo, vec![spec("new file"), spec("base")], 3)?;
        assert_eq!(
            saved(&repo, "new file")?,
            b"new\n",
            "new files are visible to command-line Git"
        );
        unstage(&repo, vec![spec("base")], 3)?;
        assert_eq!(
            saved(&repo, "new file")?,
            b"new\n",
            "unstaging another path preserves this staged file"
        );
        assert!(
            !root.join("base").exists(),
            "unstaging a deletion does not recreate the worktree file"
        );
        let status = crate::diff::staging_changes(&repo)?;
        assert_eq!(
            status.staged[0].path, "new file",
            "the addition remains staged"
        );
        assert_eq!(
            status.unstaged[0].path, "base",
            "the deletion is now unstaged"
        );
        unstage(&repo, vec![spec("new file")], 3)?;
        assert_eq!(
            std::fs::read(root.join("new file"))?,
            b"new\n",
            "unstaging a new file keeps its contents"
        );
        Ok(())
    }

    #[test]
    fn split_staging_renames_and_external_index_changes() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        invoke_bash("git mv base renamed", &repo);
        let status = crate::diff::staging_changes(&repo)?;
        assert_eq!(
            status.staged.len(),
            1,
            "external staging is visible immediately"
        );
        let change = &status.staged[0];
        unstage(
            &repo,
            vec![DiffSpec {
                path: change.path.clone(),
                previous_path: change.previous_path().map(ToOwned::to_owned),
                hunk_headers: Vec::new(),
            }],
            3,
        )?;
        assert_eq!(
            saved(&repo, "base")?,
            b"base\n",
            "unstaging restores the source entry"
        );
        assert!(
            repo.workdir().unwrap().join("renamed").exists(),
            "the renamed worktree file is preserved"
        );
        stage(
            &repo,
            vec![DiffSpec {
                path: "renamed".into(),
                previous_path: Some("base".into()),
                hunk_headers: Vec::new(),
            }],
            3,
        )?;
        assert_eq!(
            saved(&repo, "renamed")?,
            b"base\n",
            "staging records the destination"
        );
        assert!(
            crate::diff::staging_changes(&repo)?.unstaged.is_empty(),
            "the rename is fully staged"
        );
        Ok(())
    }

    #[test]
    fn split_staging_respects_index_lock() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let before = std::fs::read(repo.index_path())?;
        let _lock = lock_index(&repo)?;
        std::fs::write(repo.workdir().unwrap().join("base"), "new\n")?;
        assert!(
            stage(&repo, vec![spec("base")], 3).is_err(),
            "another Git operation owns the index"
        );
        assert_eq!(
            std::fs::read(repo.index_path())?,
            before,
            "a locked index is never replaced"
        );
        Ok(())
    }

    #[test]
    fn split_staging_uses_git_filters() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let root = repo.workdir().unwrap();
        std::fs::write(root.join(".gitattributes"), "*.txt text eol=crlf\n")?;
        std::fs::write(root.join("file.txt"), "one\r\ntwo\r\n")?;
        stage(&repo, vec![spec(".gitattributes"), spec("file.txt")], 3)?;
        assert_eq!(
            saved(&repo, "file.txt")?,
            b"one\ntwo\n",
            "staged blobs use Git's clean filter"
        );
        assert_eq!(
            std::fs::read(root.join("file.txt"))?,
            b"one\r\ntwo\r\n",
            "filters do not rewrite the worktree"
        );
        Ok(())
    }

    #[test]
    fn split_staging_preserves_unrelated_conflicts() -> anyhow::Result<()> {
        use crate::index::ConflictStage;
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let mut index = repo.index_or_empty()?.into_owned_or_cloned();
        let mut edit = Edit::new(&mut index);
        for (stage, contents) in [
            (ConflictStage::Base, "base"),
            (ConflictStage::Ours, "ours"),
            (ConflictStage::Theirs, "theirs"),
        ] {
            edit.set_conflict_stage(
                "conflict".into(),
                stage,
                gix::index::entry::Mode::FILE,
                repo.write_blob(contents)?.detach(),
            );
        }
        edit.finish();
        index.write(Default::default())?;
        std::fs::write(repo.workdir().unwrap().join("base"), "saved\n")?;
        stage(&repo, vec![spec("base")], 3)?;
        let index = repo.index_or_empty()?;
        assert_eq!(
            index
                .entries()
                .iter()
                .filter(|entry| entry.path(&index) == "conflict")
                .count(),
            3,
            "staging another file retains each conflict stage"
        );
        assert!(
            tree(&repo).is_err(),
            "unresolved index entries prevent committing"
        );
        Ok(())
    }

    #[test]
    fn split_staging_rejects_paths_outside_worktree() -> anyhow::Result<()> {
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let before = std::fs::read(repo.index_path())?;
        for path in [
            "../outside",
            "/absolute",
            ".git/config",
            "dir/../../outside",
        ] {
            assert!(
                stage(&repo, vec![spec(path)], 3).is_err(),
                "invalid staging path: {path}"
            );
        }
        assert_eq!(
            std::fs::read(repo.index_path())?,
            before,
            "invalid paths leave the index unchanged"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn split_staging_keeps_symlinks_and_modes() -> anyhow::Result<()> {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{PermissionsExt, symlink},
        };
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let root = repo.workdir().unwrap();
        std::fs::set_permissions(root.join("base"), std::fs::Permissions::from_mode(0o755))?;
        symlink("base", root.join("link"))?;
        let name = b"raw-file";
        std::fs::write(root.join(std::ffi::OsStr::from_bytes(name)), "bytes\n")?;
        stage(
            &repo,
            vec![
                spec("base"),
                spec("link"),
                DiffSpec {
                    path: name.as_slice().into(),
                    previous_path: None,
                    hunk_headers: Vec::new(),
                },
            ],
            3,
        )?;
        let index = repo.index_or_empty()?;
        assert_eq!(
            index.entry_by_path("base".into()).unwrap().mode,
            gix::index::entry::Mode::FILE_EXECUTABLE,
            "staging records the executable bit"
        );
        assert_eq!(
            index.entry_by_path("link".into()).unwrap().mode,
            gix::index::entry::Mode::SYMLINK,
            "links are stored as links"
        );
        assert!(
            index.entry_by_path(name.as_bstr()).is_some(),
            "names round-trip through staging"
        );
        assert_eq!(
            saved(&repo, "link")?,
            b"base",
            "links store their target instead of dereferencing it"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn split_staging_keeps_non_utf8_paths() -> anyhow::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let (repo, _tmp) = writable_scenario("single-unsigned");
        let path = b"raw-\xff";
        std::fs::write(
            repo.workdir()
                .unwrap()
                .join(std::ffi::OsStr::from_bytes(path)),
            "bytes\n",
        )?;
        stage(
            &repo,
            vec![DiffSpec {
                path: path.as_slice().into(),
                previous_path: None,
                hunk_headers: Vec::new(),
            }],
            3,
        )?;
        assert!(
            repo.index_or_empty()?
                .entry_by_path(path.as_bstr())
                .is_some(),
            "non-UTF-8 paths retain their original bytes"
        );
        Ok(())
    }
}
