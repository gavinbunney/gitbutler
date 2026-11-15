<script lang="ts">
	import StagingHunk from "$components/diff/StagingHunk.svelte";
	import ReduxResult from "$components/shared/ReduxResult.svelte";
	import { DIFF_SERVICE } from "$lib/hunks/diffService.svelte";
	import { inject } from "@gitbutler/core/context";
	import type { HunkHeader, TreeChange } from "@gitbutler/but-sdk";

	type Props = {
		projectId: string;
		change: TreeChange;
		side: "staged" | "unstaged";
		busy: boolean;
		onmove: (headers: HunkHeader[]) => Promise<void>;
	};
	const { projectId, change, side, busy, onmove }: Props = $props();
	const diffService = inject(DIFF_SERVICE);
	const diffQuery = $derived(diffService.getDiff(projectId, change));

	function supportsPartialStaging(change: TreeChange) {
		switch (change.status.type) {
			case "Rename":
				return false;
			case "Modification":
				return (
					change.status.subject.flags === null &&
					["Blob", "BlobExecutable"].includes(change.status.subject.state.kind)
				);
			case "Addition":
				return ["Blob", "BlobExecutable"].includes(change.status.subject.state.kind);
			case "Deletion":
				return ["Blob", "BlobExecutable"].includes(change.status.subject.previousState.kind);
		}
	}
</script>

<header>
	<strong>{change.path}</strong>
	<span>{side === "staged" ? "Staged changes" : "Unstaged changes"}</span>
</header>
<ReduxResult result={diffQuery.result}>
	{#snippet children(diff)}
		{#if diff?.type === "Patch"}
			{@const selectable =
				!diff.subject.isResultOfBinaryToTextConversion && supportsPartialStaging(change)}
			{#if selectable}
				<p class="selection-hint">
					Click a line to select changes. Shift-click or drag to select a range.
				</p>
			{/if}
			{#each diff.subject.hunks as hunk}
				{#key `${side}:${change.pathBytes.join(",")}:${selectable}:${hunk.diff}`}
					<StagingHunk filePath={change.path} {hunk} {side} {busy} {selectable} {onmove} />
				{/key}
			{:else}
				<p class="empty">No text changes</p>
			{/each}
		{:else}
			<p class="empty">
				{diff?.type === "Binary"
					? "Binary file changed"
					: diff?.type === "TooLarge"
						? "This file is too large to display"
						: "No text changes"}
			</p>
		{/if}
	{/snippet}
</ReduxResult>

<style>
	header {
		display: flex;
		flex-shrink: 0;
		align-items: center;
		justify-content: space-between;
		min-height: 36px;
		padding: 4px 12px;
		gap: 8px;
		border-bottom: 1px solid var(--border-2);
		font-size: 12px;
	}
	header strong {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	header span {
		flex-shrink: 0;
		color: var(--text-2);
	}
	.selection-hint {
		margin: 0;
		padding: 10px 12px 4px;
		color: var(--text-2);
		font-size: 11px;
	}
	.empty {
		padding: 16px;
		color: var(--text-2);
		font-size: 12px;
	}
</style>
