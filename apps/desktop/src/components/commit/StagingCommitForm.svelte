<script lang="ts">
	import Resizer from "$components/shared/Resizer.svelte";
	import { projectRunCommitHooks } from "$lib/config/config";
	import { HOOKS_SERVICE } from "$lib/git/hooksService";
	import { showWarning } from "$lib/notifications/toasts";
	import { STACK_SERVICE } from "$lib/stacks/stackService.svelte";
	import { UI_STATE } from "$lib/state/uiState.svelte";
	import { WORKTREE_SERVICE } from "$lib/worktree/worktreeService.svelte";
	import { inject } from "@gitbutler/core/context";
	import { Button, Select, SelectItem } from "@gitbutler/ui-svelte";
	import type { StagingStatus } from "@gitbutler/but-sdk";

	type Props = {
		projectId: string;
		status: StagingStatus;
		busy: boolean;
		maxHeight: number;
	};
	let { projectId, status, busy = $bindable(false), maxHeight }: Props = $props();
	const worktreeService = inject(WORKTREE_SERVICE);
	const stackService = inject(STACK_SERVICE);
	const hooksService = inject(HOOKS_SERVICE);
	const uiState = inject(UI_STATE);
	const [messageHook] = hooksService.message;
	const stacksQuery = $derived(stackService.stacks(projectId));
	const branches = $derived(
		stacksQuery.result?.data?.flatMap((stack) =>
			stack.segments.flatMap((segment) => (segment.refName ? [segment.refName] : [])),
		) ?? [],
	);
	const runCommitHooks = $derived(projectRunCommitHooks(projectId));
	const draft = $derived(uiState.lane(`index-commit-${projectId}`).newCommitMessage);
	const message = $derived(draft.current?.title ?? "");

	let target = $state("");
	let committing = $state(false);
	let commitPanel = $state<HTMLFormElement>();
	const targetBranch = $derived(
		branches.find((branch) => branch.displayName === target) ?? branches[0],
	);

	async function commit() {
		if (busy || !targetBranch || !status.indexTree || !status.staged.length || !message.trim()) {
			return;
		}

		const branch = targetBranch;
		busy = true;
		committing = true;

		try {
			let finalMessage = message;
			let expectedIndexTree = status.indexTree;

			if ($runCommitHooks) {
				await hooksService.runPreCommitHooks(projectId, "index");
				const result = await messageHook({ projectId, message: finalMessage });

				if (result?.status === "failure") {
					showWarning("Commit message hook failed", result.error);
					return;
				}

				if (result?.status === "message") finalMessage = result.message;
				const refreshed = await worktreeService.fetchStagingStatus(projectId);

				if (!refreshed?.indexTree) return;
				expectedIndexTree = refreshed.indexTree;
			}

			await worktreeService.commitIndex({
				projectId,
				target: `refs/heads/${branch.displayName}`,
				expectedIndexTree,
				message: finalMessage,
			});
			draft.set({ title: "", description: "" });

			if ($runCommitHooks) await hooksService.runPostCommitHooks(projectId);
		} finally {
			busy = false;
			committing = false;
		}
	}
</script>

<form
	class="commit-box"
	bind:this={commitPanel}
	onsubmit={(event) => {
		event.preventDefault();
		void commit();
	}}
>
	<Resizer
		viewport={commitPanel}
		direction="up"
		defaultValue={16}
		minHeight={10}
		{maxHeight}
		persistId="split-staging-commit-height"
	/>
	<label class="text-13 text-semibold" for="index-commit-message">Commit message</label>
	<textarea
		class="text-input scrollbar"
		id="index-commit-message"
		value={message}
		disabled={busy}
		placeholder="Enter a commit message"
		oninput={(event) => draft.set({ title: event.currentTarget.value, description: "" })}
		onkeydown={(event) => {
			if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
				event.preventDefault();
				void commit();
			}
		}}
	></textarea>
	<div class="commit-actions">
		<label for="index-commit-branch">Commit to</label>
		<Select
			id="index-commit-branch"
			icon="branch"
			customWidth={280}
			popupVerticalAlign="top"
			searchable
			options={branches.map((branch) => ({
				label: branch.displayName,
				value: branch.displayName,
			}))}
			value={targetBranch?.displayName ?? ""}
			disabled={busy}
			onselect={(value) => (target = value)}
		>
			{#snippet itemSnippet({ item, highlighted })}
				<SelectItem selected={item.value === targetBranch?.displayName} {highlighted}>
					{item.label}
				</SelectItem>
			{/snippet}
		</Select>
		<Button
			type="submit"
			style="pop"
			icon="commit"
			hotkey="⌘↵"
			loading={committing}
			disabled={busy ||
				!message.trim() ||
				!targetBranch ||
				!status.indexTree ||
				status.staged.length === 0}
		>
			Commit staged files
		</Button>
	</div>
</form>

<style>
	.commit-box {
		box-sizing: border-box;
		display: flex;
		position: relative;
		flex: 0 0 auto;
		flex-direction: column;
		padding: 12px 16px;
		gap: 8px;
		border-top: 1px solid var(--border-2);
		font-size: 12px;
	}
	.commit-box::before {
		position: absolute;
		top: -2px;
		left: calc(50% - 18px);
		width: 36px;
		height: 3px;
		border-radius: 2px;
		background: var(--border-2);
		content: "";
	}
	textarea {
		flex: 1;
		width: 100%;
		min-height: 0;
		padding: 8px;
		font-size: 13px;
		line-height: 1.6;
		resize: none;
	}
	.commit-actions {
		display: flex;
		align-items: center;
		gap: 8px;
	}
	.commit-actions :global(.select-wrapper) {
		width: 240px;
		min-width: 0;
		margin-right: auto;
	}
</style>
