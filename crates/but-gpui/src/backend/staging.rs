//! The shared Git index, independent of the composer's worktree selections.

use std::{path::Path, sync::Arc};

use anyhow::{Context as _, Result, ensure};
use but_core::DiffSpec;
use gitbutler_repo::hooks::{ErrorData, HookResult, MessageHookResult};

use super::{
    FileChange, FileDiff, FullName, Intent, Selected, diff, file_change, intent::Run, open, ops,
    stamp,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Unstaged,
    Staged,
}

impl Side {
    pub fn action(self) -> &'static str {
        match self {
            Self::Unstaged => "Stage",
            Self::Staged => "Unstage",
        }
    }
}

pub struct Status {
    pub staged: Vec<FileDiff>,
    pub unstaged: Vec<FileDiff>,
    pub index_tree: Option<gix::ObjectId>,
    pub conflicts: usize,
}

impl Status {
    pub fn files(&self, side: Side) -> &[FileDiff] {
        match side {
            Side::Unstaged => &self.unstaged,
            Side::Staged => &self.staged,
        }
    }
}

/// Whether the displayed patch supports hunk and line actions.
pub fn supports_partial(file: &FileDiff) -> bool {
    use but_core::TreeStatus;
    use gix::object::tree::EntryKind;

    let plain = |state: &but_core::ChangeState| {
        matches!(state.kind, EntryKind::Blob | EntryKind::BlobExecutable)
    };
    !file.file.converted
        && !file.hunks().is_empty()
        && match &file.file.change.status {
            TreeStatus::Addition { state, .. }
            | TreeStatus::Deletion {
                previous_state: state,
            } => plain(state),
            TreeStatus::Modification {
                previous_state,
                state,
                flags: None,
            } => plain(previous_state) && plain(state),
            TreeStatus::Modification { flags: Some(_), .. } | TreeStatus::Rename { .. } => false,
        }
}

fn staging_file(repo: &gix::Repository, change: but_core::TreeChange, side: Side) -> FileChange {
    let mut file = file_change(change);
    if side == Side::Unstaged {
        (file.modified, file.size) = stamp(repo, file.change.path.as_ref());
    }
    file
}

/// Read both lists under one permission, keeping highlighted files whose patches didn't change.
pub fn load(path: &Path, previous: Option<Arc<Status>>, scope: Option<usize>) -> Result<Status> {
    let ctx = open(path)?;
    let guard = ctx.shared_worktree_access();
    let status = but_api::staging::staging_status(&ctx, guard.read_permission())?;
    let repo = ctx.repo.get()?;
    let files = |changes: Vec<but_core::ui::TreeChange>, side| -> Result<Vec<FileDiff>> {
        changes
            .into_iter()
            .map(|change| {
                let file = staging_file(&repo, change.into(), side);
                let old = previous.as_ref().and_then(|old| {
                    old.files(side)
                        .iter()
                        .find(|old| old.file.change.path == file.change.path)
                });
                diff::load_file(
                    &repo,
                    file,
                    ctx.settings.context_lines,
                    old,
                    Some(12_000),
                    scope,
                    false,
                )
            })
            .collect()
    };
    Ok(Status {
        staged: files(status.staged, Side::Staged)?,
        unstaged: files(status.unstaged, Side::Unstaged)?,
        index_tree: status.index_tree,
        conflicts: status
            .ignored_changes
            .iter()
            .filter(|change| change.status == but_core::IgnoredWorktreeTreeChangeStatus::Conflict)
            .count(),
    })
}

/// A displayed file or part of it, with the patch it was selected from.
#[derive(Clone)]
pub struct Pick {
    pub changes: Selected,
    pub fingerprint: u64,
}

pub struct ChangeIndex {
    pub side: Side,
    pub picks: Vec<Pick>,
    pub index_tree: gix::ObjectId,
}

impl Intent for ChangeIndex {
    type Done = ();

    fn apply(self, run: &mut Run) -> Result<()> {
        let Self {
            side,
            picks,
            index_tree,
        } = self;
        ensure!(!picks.is_empty(), "Select changes first");
        run.locked(|ctx, perm| {
            let specs = checked_specs(ctx, perm.read_permission(), side, &picks, index_tree)?;
            match side {
                Side::Unstaged => but_api::staging::staging_stage(ctx, specs, perm),
                Side::Staged => but_api::staging::staging_unstage(ctx, specs, perm),
            }
        })
    }
}

fn checked_specs(
    ctx: &but_ctx::Context,
    perm: &but_ctx::access::RepoShared,
    side: Side,
    picks: &[Pick],
    index_tree: gix::ObjectId,
) -> Result<Vec<DiffSpec>> {
    let status = but_api::staging::staging_status(ctx, perm)?;
    ensure!(
        status.index_tree == Some(index_tree),
        "The staging area changed. Review the files and try again"
    );
    let current = match side {
        Side::Unstaged => status.unstaged,
        Side::Staged => status.staged,
    };
    let repo = ctx.repo.get()?;
    picks
        .iter()
        .map(
            |Pick {
                 changes,
                 fingerprint,
             }| {
                let Selected {
                    file,
                    hunks,
                    made_by: _,
                } = changes;
                let current = current
                    .iter()
                    .find(|change| *change.path == file.change.path)
                    .context("The selected file changed. Review it and try again")?;
                let now = diff::load_file(
                    &repo,
                    staging_file(&repo, current.clone().into(), side),
                    ctx.settings.context_lines,
                    None,
                    None,
                    None,
                    false,
                )?;
                ensure!(
                    now.fingerprint == *fingerprint,
                    "{} changed. Review it and try again",
                    file.path
                );
                ensure!(
                    hunks.is_none() || supports_partial(&now),
                    "Use the whole file: this diff does not support partial changes"
                );
                let mut spec = DiffSpec::from(&file.change);
                if let Some(hunks) = hunks {
                    ensure!(!hunks.is_empty(), "Select changed lines first");
                    spec.hunk_headers = hunks.iter().map(|key| key.0).collect();
                }
                Ok(spec)
            },
        )
        .collect()
}

pub struct DiscardUnstaged {
    pub pick: Pick,
    pub index_tree: gix::ObjectId,
}

impl Intent for DiscardUnstaged {
    /// A write failure after recording undo; the snapshot must remain available.
    type Done = Option<String>;

    fn apply(self, run: &mut Run) -> Result<Self::Done> {
        let Self { pick, index_tree } = self;
        ensure!(
            pick.changes
                .hunks
                .as_ref()
                .is_some_and(|hunks| !hunks.is_empty()),
            "Select hunks or lines to discard"
        );
        run.locked(|ctx, perm| {
            checked_specs(
                ctx,
                perm.read_permission(),
                Side::Unstaged,
                std::slice::from_ref(&pick),
                index_tree,
            )?;
            let details = but_oplog::legacy::SnapshotDetails::new(
                but_oplog::legacy::OperationKind::DiscardChanges,
            );
            but_oplog::UnmaterializedOplogSnapshot::prepare_checkpoint(
                ctx,
                details,
                &[],
                perm.read_permission(),
            )
            .and_then(|snapshot| snapshot.commit(ctx, perm))
            .context("The lines weren't discarded because GitButler couldn't record undo")?;
            // the snapshot can take time; refuse disk or index edits made while recording it
            let discarded = (|| -> Result<()> {
                let spec = checked_specs(
                    ctx,
                    perm.read_permission(),
                    Side::Unstaged,
                    std::slice::from_ref(&pick),
                    index_tree,
                )?
                .pop()
                .context("Select changes first")?;
                but_core::staging::discard(
                    &*ctx.repo.get()?,
                    &spec,
                    index_tree,
                    ctx.settings.context_lines,
                )
            })();
            Ok(discarded.err().map(|err| format!("{err:#}")))
        })
    }
}

pub struct CommitIndex {
    pub target: FullName,
    pub index_tree: gix::ObjectId,
    pub message: String,
    pub run_hooks: bool,
}

impl Intent for CommitIndex {
    /// A post-commit hook failure: the commit still succeeded.
    type Done = Option<String>;

    fn apply(self, run: &mut Run) -> Result<Self::Done> {
        let Self {
            target,
            index_tree,
            message,
            run_hooks,
        } = self;
        ensure!(!message.trim().is_empty(), "Write a commit message first");
        run.locked(|ctx, perm| {
            ops::refuse_unfinished(&*ctx.repo.get()?, true)?;
            let status = but_api::staging::staging_status(ctx, perm.read_permission())?;
            ensure!(
                status.index_tree == Some(index_tree),
                "The staging area changed. Review the staged files before committing again"
            );
            ensure!(!status.staged.is_empty(), "Stage changes first");
            let message = if run_hooks {
                if let HookResult::Failure(ErrorData { error }) =
                    but_api::legacy::repo::pre_commit_hook_index(ctx, perm)?
                {
                    anyhow::bail!(
                        "The pre-commit hook refused the commit:\n{}",
                        ops::hook_output(&error)
                    );
                }
                match but_api::legacy::repo::message_hook(ctx, format!("{message}\n"))? {
                    MessageHookResult::Failure(ErrorData { error }) => anyhow::bail!(
                        "The commit-msg hook refused the commit:\n{}",
                        ops::hook_output(&error)
                    ),
                    MessageHookResult::Message(rewritten) => rewritten.message,
                    MessageHookResult::Success | MessageHookResult::NotConfigured => message,
                }
            } else {
                message
            };
            // hooks may deliberately stage fixes; commit the index they leave behind
            let tree = but_core::staging::tree(&*ctx.repo.get()?)?;
            but_api::staging::staging_commit(ctx, target.as_ref(), tree, message, perm)?;
            Ok(())
        })?;
        Ok(run_hooks.then(|| ops::post_commit(run.ctx())).flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        apply,
        model::build::full,
        ops::tests::{git, repo_with, target_origin_main},
    };

    fn selection(status: &Status, side: Side) -> ChangeIndex {
        ChangeIndex {
            side,
            index_tree: status.index_tree.expect("the test index has no conflicts"),
            picks: status
                .files(side)
                .iter()
                .map(|file| Pick {
                    changes: Selected::whole(&file.file),
                    fingerprint: file.fingerprint,
                })
                .collect(),
        }
    }

    #[test]
    #[cfg(unix)]
    fn partial_actions_require_regular_files_without_mode_changes() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        but_testsupport::isolated_app_data_dir(|| {
            let dir = repo_with("base\n");
            let path = dir.path();
            std::fs::write(path.join("file"), "changed\n").unwrap();
            let status = load(path, None, None).unwrap();
            assert!(
                supports_partial(&status.unstaged[0]),
                "regular text supports partial actions"
            );
            std::fs::set_permissions(path.join("file"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            symlink("file", path.join("link")).unwrap();
            let status = load(path, None, None).unwrap();
            assert!(
                status.unstaged.iter().all(|file| !supports_partial(file)),
                "mode changes and symlinks require whole-file actions"
            );
        });
    }

    #[test]
    fn discards_unstaged_lines_without_losing_saved_content_and_can_undo() {
        but_testsupport::isolated_app_data_dir(|| {
            let dir = repo_with("base\n");
            let path = dir.path();
            target_origin_main(path);
            std::fs::write(path.join("file"), "base\nsaved\n").unwrap();
            git(path, &["add", "file"]);
            std::fs::write(path.join("file"), "base\nsaved\ndiscard\nkeep\n").unwrap();
            let status = load(path, None, None).unwrap();
            let file = &status.unstaged[0];
            let hunk = &file.hunks()[0];
            let line = hunk
                .lines
                .iter()
                .position(|line| &*line.text == "discard")
                .unwrap();
            let mut changes = Selected::whole(&file.file);
            changes.hunks = Some(hunk.picked(&[line].into()));
            let (outcome, ran) = apply(
                path,
                DiscardUnstaged {
                    pick: Pick {
                        changes,
                        fingerprint: file.fingerprint,
                    },
                    index_tree: status.index_tree.unwrap(),
                },
            )
            .unwrap();
            assert!(
                outcome.is_none(),
                "the displayed index-relative lines can be discarded"
            );
            assert_eq!(
                git(path, &["show", ":file"]),
                "base\nsaved\n",
                "discard preserves the saved index"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file")).unwrap(),
                "base\nsaved\nkeep\n",
                "only the selected unstaged line is removed"
            );
            let snapshot = ran.recorded.expect("discard records undo before writing");
            let (undone, _) = apply(
                path,
                crate::backend::history::Undo {
                    snapshot,
                    newer_than: None,
                },
            )
            .unwrap();
            assert!(
                matches!(undone.unwrap(), crate::backend::history::Undone::Done(_)),
                "discard can be undone"
            );
            assert_eq!(
                git(path, &["show", ":file"]),
                "base\nsaved\n",
                "undo retains the saved index"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file")).unwrap(),
                "base\nsaved\ndiscard\nkeep\n",
                "undo restores the discarded line"
            );
        });
    }

    #[test]
    fn discards_only_the_requested_unstaged_hunk() {
        but_testsupport::isolated_app_data_dir(|| {
            let dir = repo_with("base\n");
            let path = dir.path();
            target_origin_main(path);
            let saved = (1..=30)
                .map(|line| format!("line {line}\n"))
                .collect::<String>();
            std::fs::write(path.join("file"), &saved).unwrap();
            git(path, &["add", "file"]);
            let disk = saved
                .replace("line 2\n", "discard\n")
                .replace("line 29\n", "keep\n");
            std::fs::write(path.join("file"), &disk).unwrap();
            let status = load(path, None, None).unwrap();
            let file = &status.unstaged[0];
            assert_eq!(file.hunks().len(), 2, "the edits are in separate hunks");
            let mut changes = Selected::whole(&file.file);
            changes.hunks = Some(vec![file.hunks()[0].key]);
            let (error, _) = apply(
                path,
                DiscardUnstaged {
                    pick: Pick {
                        changes,
                        fingerprint: file.fingerprint,
                    },
                    index_tree: status.index_tree.unwrap(),
                },
            )
            .unwrap();
            assert!(error.is_none(), "the selected hunk is discarded");
            assert_eq!(
                git(path, &["show", ":file"]),
                saved,
                "the saved index is unchanged"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file")).unwrap(),
                saved.replace("line 29\n", "keep\n"),
                "another hunk's edit remains on disk"
            );
        });
    }

    #[test]
    fn commits_the_saved_index_and_keeps_later_edits() {
        but_testsupport::isolated_app_data_dir(|| {
            let dir = repo_with("base\n");
            let path = dir.path();
            target_origin_main(path);
            git(path, &["checkout", "-q", "-b", "feature"]);
            std::fs::write(path.join("file"), "saved\n").unwrap();
            let (_, staged) = apply(
                path,
                selection(&load(path, None, None).unwrap(), Side::Unstaged),
            )
            .unwrap();
            assert!(staged.recorded.is_some(), "staging records an undo entry");
            std::fs::write(path.join("file"), "later\n").unwrap();
            let status = load(path, None, None).unwrap();
            assert_eq!(
                (status.staged.len(), status.unstaged.len()),
                (1, 1),
                "one file can appear on both sides"
            );
            let (_, committed) = apply(
                path,
                CommitIndex {
                    target: full("refs/heads/feature"),
                    index_tree: status.index_tree.unwrap(),
                    message: "Saved version".into(),
                    run_hooks: false,
                },
            )
            .unwrap();
            assert!(
                committed.recorded.is_some(),
                "the commit records an undo entry"
            );
            assert_eq!(
                git(path, &["show", "HEAD:file"]),
                "saved\n",
                "the commit uses the saved content"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file")).unwrap(),
                "later\n",
                "committing preserves disk edits"
            );
            let status = load(path, None, None).unwrap();
            assert!(status.staged.is_empty(), "the committed index is clean");
            assert_eq!(status.unstaged.len(), 1, "later edits stay unstaged");
        });
    }

    #[test]
    fn undo_restores_the_index_without_changing_worktree_content() {
        but_testsupport::isolated_app_data_dir(|| {
            let dir = repo_with("base\n");
            let path = dir.path();
            target_origin_main(path);
            std::fs::write(path.join("file"), "saved\n").unwrap();
            let (_, ran) = apply(
                path,
                selection(&load(path, None, None).unwrap(), Side::Unstaged),
            )
            .unwrap();
            let snapshot = ran
                .recorded
                .expect("staging in a targeted repository records undo");
            let (undone, _) = apply(
                path,
                crate::backend::history::Undo {
                    snapshot,
                    newer_than: None,
                },
            )
            .unwrap();
            assert!(
                matches!(undone.unwrap(), crate::backend::history::Undone::Done(_)),
                "the native undo operation restores staging"
            );
            assert_eq!(
                git(path, &["show", ":file"]),
                "base\n",
                "undo restores the previous index"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file")).unwrap(),
                "saved\n",
                "undo keeps the worktree content"
            );
        });
    }

    #[test]
    fn stages_and_unstages_selected_lines_without_touching_disk() {
        but_testsupport::isolated_app_data_dir(|| {
            let dir = repo_with("base\n");
            let path = dir.path();
            std::fs::write(path.join("file"), "base\none\ntwo\n").unwrap();
            let status = load(path, None, None).unwrap();
            let file = &status.unstaged[0];
            let hunk = &file.hunks()[0];
            let line = hunk
                .lines
                .iter()
                .position(|line| &*line.text == "one")
                .unwrap();
            let mut intent = selection(&status, Side::Unstaged);
            intent.picks[0].changes.hunks = Some(hunk.picked(&[line].into()));
            apply(path, intent).unwrap();
            assert_eq!(
                git(path, &["show", ":file"]),
                "base\none\n",
                "only the selected line is staged"
            );
            let status = load(path, None, None).unwrap();
            let key = status.staged[0].hunks()[0].key;
            let mut intent = selection(&status, Side::Staged);
            intent.picks[0].changes.hunks = Some(vec![key]);
            apply(path, intent).unwrap();
            assert_eq!(
                git(path, &["show", ":file"]),
                "base\n",
                "unstaging a hunk restores the index"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file")).unwrap(),
                "base\none\ntwo\n",
                "neither operation edits the worktree"
            );
        });
    }

    #[test]
    fn refuses_files_and_index_changed_since_they_were_shown() {
        but_testsupport::isolated_app_data_dir(|| {
            let dir = repo_with("base\n");
            let path = dir.path();
            std::fs::write(path.join("file"), "shown\n").unwrap();
            let status = load(path, None, None).unwrap();
            let discard = || {
                let file = &status.unstaged[0];
                let mut changes = Selected::whole(&file.file);
                changes.hunks = Some(vec![file.hunks()[0].key]);
                DiscardUnstaged {
                    pick: Pick {
                        changes,
                        fingerprint: file.fingerprint,
                    },
                    index_tree: status.index_tree.unwrap(),
                }
            };
            std::fs::write(path.join("file"), "new edit\n").unwrap();
            assert!(
                apply(path, discard()).is_err(),
                "discard refuses edits made since the diff was shown"
            );
            assert!(
                apply(path, selection(&status, Side::Unstaged)).is_err(),
                "stale file selections are refused"
            );
            git(path, &["add", "file"]);
            assert!(
                apply(path, discard()).is_err(),
                "discard refuses an index changed since the diff was shown"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file")).unwrap(),
                "new edit\n",
                "a refused discard leaves disk alone"
            );
            assert!(
                apply(path, selection(&status, Side::Unstaged)).is_err(),
                "external staging invalidates the displayed index"
            );
            assert!(
                apply(
                    path,
                    CommitIndex {
                        target: full("refs/heads/main"),
                        index_tree: status.index_tree.unwrap(),
                        message: "Stale".into(),
                        run_hooks: false
                    }
                )
                .is_err(),
                "a stale index cannot be committed"
            );
            assert_eq!(
                git(path, &["log", "-1", "--format=%s"]).trim(),
                "first",
                "nothing was committed"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn hooks_read_the_index_and_can_stage_fixes() {
        but_testsupport::isolated_app_data_dir(|| {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = repo_with("base\n");
            let path = dir.path();
            std::fs::write(path.join("file"), "saved\n").unwrap();
            apply(
                path,
                selection(&load(path, None, None).unwrap(), Side::Unstaged),
            )
            .unwrap();
            std::fs::write(path.join("file"), "later\n").unwrap();
            let hook = |name: &str, script: &str| {
                let file = path.join(".git/hooks").join(name);
                std::fs::write(&file, format!("#!/bin/sh\n{script}\n")).unwrap();
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            };
            let commit = || CommitIndex {
                target: full("refs/heads/main"),
                index_tree: load(path, None, None).unwrap().index_tree.unwrap(),
                message: "Index hooks".into(),
                run_hooks: true,
            };
            hook("pre-commit", "echo 'hook refused'; exit 1");
            assert!(
                apply(path, commit()).is_err(),
                "a failed pre-commit hook prevents the commit"
            );
            hook(
                "pre-commit",
                "test \"$(git show :file)\" = saved || exit 1\nprintf 'fixed\\n' > fixed\ngit add fixed",
            );
            hook("commit-msg", "printf '\\nReviewed\\n' >> \"$1\"");
            hook("post-commit", "echo 'notification failed'; exit 1");
            let (warning, _) = apply(path, commit()).unwrap();
            assert!(
                warning.is_some(),
                "post-commit failure is reported after success"
            );
            assert_eq!(
                git(path, &["show", "HEAD:fixed"]),
                "fixed\n",
                "hook-staged fixes are committed"
            );
            assert_eq!(
                git(path, &["show", "HEAD:file"]),
                "saved\n",
                "hooks see the saved version"
            );
            assert!(
                git(path, &["log", "-1", "--format=%B"]).contains("Reviewed"),
                "commit-msg edits are retained"
            );
            assert_eq!(
                std::fs::read_to_string(path.join("file")).unwrap(),
                "later\n",
                "unstaged edits survive hooks"
            );
        });
    }
}
