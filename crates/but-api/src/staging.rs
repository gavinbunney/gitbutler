use anyhow::ensure;
use but_api_macros::but_api;
use but_core::{
    DiffSpec, DryRun,
    sync::{RepoExclusive, RepoShared},
};
use but_oplog::legacy::{OperationKind, SnapshotDetails};
use but_transaction::{Commit, with_transaction_with_perm};

pub mod json {
    use but_core::{IgnoredWorktreeChange, ui::TreeChange};
    use serde::Serialize;

    /// Independent changes on each side of Git's shared index.
    #[derive(Debug, Serialize)]
    #[cfg_attr(feature = "export-schema", derive(schemars::JsonSchema))]
    #[serde(rename_all = "camelCase")]
    pub struct StagingStatus {
        pub staged: Vec<TreeChange>,
        pub unstaged: Vec<TreeChange>,
        pub ignored_changes: Vec<IgnoredWorktreeChange>,
        /// Absent while the index contains unresolved conflicts.
        #[serde(with = "but_serde::object_id_opt")]
        #[cfg_attr(
            feature = "export-schema",
            schemars(schema_with = "but_schemars::object_id_opt")
        )]
        pub index_tree: Option<gix::ObjectId>,
    }
    #[cfg(feature = "export-schema")]
    but_schemars::register_sdk_type!(StagingStatus);
}

/// Read the HEAD-to-index and index-to-worktree changes separately.
#[but_api]
pub fn staging_status(
    ctx: &but_ctx::Context,
    _perm: &RepoShared,
) -> anyhow::Result<json::StagingStatus> {
    let repo = ctx.repo.get()?;
    let but_core::staging::Status {
        staged,
        unstaged,
        ignored,
    } = but_core::diff::staging_changes(&repo)?;
    let index_tree = if ignored
        .iter()
        .any(|change| change.status == but_core::IgnoredWorktreeTreeChangeStatus::Conflict)
    {
        None
    } else {
        Some(but_core::staging::tree(&repo)?)
    };
    Ok(json::StagingStatus {
        staged: staged.into_iter().map(Into::into).collect(),
        unstaged: unstaged.into_iter().map(Into::into).collect(),
        ignored_changes: ignored,
        index_tree,
    })
}

/// Save files or selected hunks and lines in Git's index without changing the worktree.
#[but_api(refreshes)]
pub fn staging_stage(
    ctx: &mut but_ctx::Context,
    changes: Vec<DiffSpec>,
    perm: &mut RepoExclusive,
) -> anyhow::Result<()> {
    let mut details = SnapshotDetails::new(OperationKind::FileChanges);
    details.title = "Stage files".into();
    let snapshot = but_oplog::UnmaterializedOplogSnapshot::from_details_with_perm(
        ctx,
        details,
        perm.read_permission(),
        DryRun::No,
    );
    but_core::staging::stage(&*ctx.repo.get()?, changes, ctx.settings.context_lines)?;
    if let Some(snapshot) = snapshot {
        snapshot.commit(ctx, perm).ok();
    }
    Ok(())
}

/// Restore files or selected index hunks and lines from HEAD without changing the worktree.
#[but_api(refreshes)]
pub fn staging_unstage(
    ctx: &mut but_ctx::Context,
    changes: Vec<DiffSpec>,
    perm: &mut RepoExclusive,
) -> anyhow::Result<()> {
    let mut details = SnapshotDetails::new(OperationKind::FileChanges);
    details.title = "Unstage files".into();
    let snapshot = but_oplog::UnmaterializedOplogSnapshot::from_details_with_perm(
        ctx,
        details,
        perm.read_permission(),
        DryRun::No,
    );
    but_core::staging::unstage(&*ctx.repo.get()?, changes, ctx.settings.context_lines)?;
    if let Some(snapshot) = snapshot {
        snapshot.commit(ctx, perm).ok();
    }
    Ok(())
}

/// Commit the saved index delta onto a branch in the current graph.
///
/// `expected_index_tree` prevents committing a different selection when another
/// client changes the index after it was displayed. Unstaged edits stay on disk.
#[but_api(try_from = crate::commit::json::CommitCreateResult, refreshes)]
pub fn staging_commit(
    ctx: &mut but_ctx::Context,
    target: &gix::refs::FullNameRef,
    expected_index_tree: gix::ObjectId,
    message: String,
    perm: &mut RepoExclusive,
) -> anyhow::Result<crate::commit::types::CommitCreateResult> {
    let (base, staged) = {
        let repo = ctx.repo.get()?;
        let staged = but_core::staging::tree(&repo)?;
        ensure!(
            staged == expected_index_tree,
            "The staging area changed. Review the staged files before committing again"
        );
        (repo.head_tree_id_or_empty()?.detach(), staged)
    };
    let mut meta = ctx.meta()?;
    let (commit, workspace) = with_transaction_with_perm(
        ctx,
        &mut meta,
        perm,
        SnapshotDetails::new(OperationKind::CreateCommit),
        &[target],
        DryRun::No,
        |mut tx| {
            Ok(Commit(
                tx.create_commit_from_tree(base, staged, target, &message)?,
            ))
        },
    )?;
    Ok(crate::commit::types::CommitCreateResult {
        new_commit: Some(commit.id),
        rejected_specs: Vec::new(),
        workspace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use but_testsupport::{CommandExt, git_at_dir, invoke_bash_at_dir, open_repo};

    fn setup() -> anyhow::Result<(but_ctx::Context, tempfile::TempDir)> {
        let tmp = tempfile::tempdir()?;
        git_at_dir(tmp.path()).args(["init", "-b", "main"]).run();
        invoke_bash_at_dir(
            "git config user.name Tester\ngit config user.email test@example.com\nprintf 'base\\n' > file.txt\ngit add .\ngit commit -m base\ngit checkout -b feature\ngit commit --allow-empty -m feature",
            tmp.path(),
        );
        let repo = open_repo(tmp.path())?;
        but_core::ref_metadata::ProjectMeta {
            target_ref: Some(but_core::ref_metadata::LocalRefName::new(
                "refs/heads/main".try_into()?,
            )?),
            target_commit_id: Some(repo.rev_parse_single("main")?.detach()),
            push_remote: None,
        }
        .persist(&repo)?;
        Ok((
            but_ctx::Context::from_repo_for_testing(repo)?.with_memory_app_cache(),
            tmp,
        ))
    }

    fn file() -> Vec<DiffSpec> {
        vec![DiffSpec {
            path: "file.txt".into(),
            previous_path: None,
            hunk_headers: Vec::new(),
        }]
    }

    #[test]
    fn split_staging_commit_saves_index_and_preserves_later_edits() -> anyhow::Result<()> {
        let (mut ctx, tmp) = setup()?;
        let mut guard = ctx.exclusive_worktree_access();
        std::fs::write(tmp.path().join("file.txt"), "staged\n")?;
        staging_stage(&mut ctx, file(), guard.write_permission())?;
        let staged = staging_status(&ctx, guard.read_permission())?
            .index_tree
            .unwrap();
        std::fs::write(tmp.path().join("file.txt"), "later\n")?;
        let result = staging_commit(
            &mut ctx,
            "refs/heads/feature".try_into()?,
            staged,
            "saved version".into(),
            guard.write_permission(),
        )?;
        let repo = ctx.repo.get()?;
        let commit = result
            .new_commit
            .expect("a nonempty staging area creates a commit");
        assert_eq!(
            repo.head_id()?.detach(),
            commit,
            "the selected branch advances to the new commit"
        );
        let blob = repo
            .rev_parse_single("HEAD:file.txt")?
            .object()?
            .try_into_blob()?;
        assert_eq!(blob.data, b"staged\n", "commits contain the saved version");
        drop(blob);
        assert_eq!(
            std::fs::read(tmp.path().join("file.txt"))?,
            b"later\n",
            "later worktree edits survive materialization"
        );
        drop(repo);
        let status = staging_status(&ctx, guard.read_permission())?;
        assert!(status.staged.is_empty(), "the committed index is clean");
        assert_eq!(status.unstaged.len(), 1, "later edits stay unstaged");
        Ok(())
    }

    #[test]
    fn split_staging_refuses_stale_selection_and_empty_message() -> anyhow::Result<()> {
        let (mut ctx, tmp) = setup()?;
        let mut guard = ctx.exclusive_worktree_access();
        std::fs::write(tmp.path().join("file.txt"), "staged\n")?;
        staging_stage(&mut ctx, file(), guard.write_permission())?;
        let stale = staging_status(&ctx, guard.read_permission())?
            .index_tree
            .unwrap();
        std::fs::write(tmp.path().join("file.txt"), "changed\n")?;
        git_at_dir(tmp.path()).args(["add", "file.txt"]).run();
        let head = ctx.repo.get()?.head_id()?.detach();
        assert!(
            staging_commit(
                &mut ctx,
                "refs/heads/feature".try_into()?,
                stale,
                "stale".into(),
                guard.write_permission()
            )
            .is_err(),
            "a different staged version must be reviewed"
        );
        let current = staging_status(&ctx, guard.read_permission())?
            .index_tree
            .unwrap();
        assert!(
            staging_commit(
                &mut ctx,
                "refs/heads/feature".try_into()?,
                current,
                " \n".into(),
                guard.write_permission()
            )
            .is_err(),
            "blank messages cannot create commits"
        );
        assert_eq!(
            ctx.repo.get()?.head_id()?.detach(),
            head,
            "failed commits leave refs untouched"
        );
        Ok(())
    }

    #[test]
    fn split_staging_commit_preserves_recreated_deletion() -> anyhow::Result<()> {
        let (mut ctx, tmp) = setup()?;
        let mut guard = ctx.exclusive_worktree_access();
        std::fs::remove_file(tmp.path().join("file.txt"))?;
        staging_stage(&mut ctx, file(), guard.write_permission())?;
        let staged = staging_status(&ctx, guard.read_permission())?
            .index_tree
            .unwrap();
        std::fs::write(tmp.path().join("file.txt"), "recreated\n")?;
        staging_commit(
            &mut ctx,
            "refs/heads/feature".try_into()?,
            staged,
            "delete saved file".into(),
            guard.write_permission(),
        )?;
        assert_eq!(
            std::fs::read(tmp.path().join("file.txt"))?,
            b"recreated\n",
            "committing a deletion retains the later recreation"
        );
        assert!(
            staging_status(&ctx, guard.read_permission())?
                .staged
                .is_empty(),
            "the saved deletion is consumed"
        );
        Ok(())
    }

    #[test]
    fn split_staging_commit_into_workspace_branch() -> anyhow::Result<()> {
        let (mut ctx, tmp) = setup()?;
        crate::branch::branch_create(
            &mut ctx,
            Some("refs/heads/other".try_into()?),
            crate::branch::json::BranchCreatePlacement::Independent { order: None },
        )?;
        let mut guard = ctx.exclusive_worktree_access();
        let initial_feature = ctx.repo.get()?.rev_parse_single("feature")?.detach();
        std::fs::write(tmp.path().join("new.txt"), "saved\n")?;
        let changes = vec![DiffSpec {
            path: "new.txt".into(),
            previous_path: None,
            hunk_headers: Vec::new(),
        }];
        staging_stage(&mut ctx, changes, guard.write_permission())?;
        let staged = staging_status(&ctx, guard.read_permission())?
            .index_tree
            .unwrap();
        std::fs::write(tmp.path().join("new.txt"), "later\n")?;
        let outcome = staging_commit(
            &mut ctx,
            "refs/heads/other".try_into()?,
            staged,
            "saved in other".into(),
            guard.write_permission(),
        )?;
        let repo = ctx.repo.get()?;
        let new_commit = outcome.new_commit.unwrap();
        assert_eq!(
            repo.rev_parse_single("other")?.detach(),
            new_commit,
            "the selected workspace branch advances"
        );
        assert_eq!(
            repo.rev_parse_single("feature")?.detach(),
            initial_feature,
            "the other lane keeps its history"
        );
        assert_eq!(
            repo.head_name()?.unwrap().as_bstr(),
            "refs/heads/gitbutler/workspace",
            "the workspace remains checked out"
        );
        assert_eq!(
            repo.rev_parse_single("other:new.txt")?
                .object()?
                .try_into_blob()?
                .data,
            b"saved\n",
            "the branch receives only index content"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("new.txt"))?,
            b"later\n",
            "workspace materialization preserves later edits"
        );
        drop(repo);
        assert!(
            staging_status(&ctx, guard.read_permission())?
                .staged
                .is_empty(),
            "the saved changes have been consumed"
        );
        Ok(())
    }

    #[test]
    fn split_staging_undo_restores_index_and_worktree() -> anyhow::Result<()> {
        use gitbutler_oplog::{OplogExt, RestoreKind};
        let (mut ctx, tmp) = setup()?;
        let mut guard = ctx.exclusive_worktree_access();
        std::fs::write(tmp.path().join("file.txt"), "staged\n")?;
        staging_stage(&mut ctx, file(), guard.write_permission())?;
        let saved = staging_status(&ctx, guard.read_permission())?
            .index_tree
            .unwrap();
        std::fs::write(tmp.path().join("file.txt"), "later\n")?;
        staging_commit(
            &mut ctx,
            "refs/heads/feature".try_into()?,
            saved,
            "save".into(),
            guard.write_permission(),
        )?;
        let snapshot = ctx.oplog_head()?.expect("committing records an undo point");
        ctx.restore_snapshot(
            snapshot,
            RestoreKind::RestoreFromSnapshotViaUndo,
            guard.write_permission(),
        )?;
        assert_eq!(
            staging_status(&ctx, guard.read_permission())?.index_tree,
            Some(saved),
            "undo restores the saved staging area"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("file.txt"))?,
            b"later\n",
            "undo preserves the later worktree edits"
        );
        Ok(())
    }
}
