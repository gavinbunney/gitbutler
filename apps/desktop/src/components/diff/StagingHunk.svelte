<script lang="ts">
	import { lineIdsToHunkHeaders } from "$lib/hunks/hunk";
	import { UI_STATE } from "$lib/state/uiState.svelte";
	import { inject } from "@gitbutler/core/context";
	import { Button, HunkDiff } from "@gitbutler/ui-svelte";
	import { lineIdKey } from "@gitbutler/ui-svelte/utils/diffParsing";
	import type { DiffHunk, HunkHeader } from "@gitbutler/but-sdk";
	import type { LineClickParams } from "@gitbutler/ui-svelte";
	import type { LineId } from "@gitbutler/ui-svelte/utils/diffParsing";

	type Props = {
		filePath: string;
		hunk: DiffHunk;
		side: "staged" | "unstaged";
		busy: boolean;
		selectable: boolean;
		onmove: (headers: HunkHeader[]) => Promise<void>;
	};
	const { filePath, hunk, side, busy, selectable, onmove }: Props = $props();
	const uiState = inject(UI_STATE);
	let lines = $state<LineId[]>([]);
	let anchor = $state<number>();
	const action = $derived(side === "staged" ? "Unstage" : "Stage");

	function moveHunk() {
		const { oldStart, oldLines, newStart, newLines } = hunk;
		return onmove([{ oldStart, oldLines, newStart, newLines }]);
	}

	function selectLines({ index, startIndex, rows, shift, ctrlOrMeta }: LineClickParams) {
		if (busy || !selectable || !rows) return;
		const start = shift && anchor !== undefined ? anchor : startIndex;
		const selected = rows
			.slice(Math.min(start, index), Math.max(start, index) + 1)
			.filter((row) => row.isDeltaLine)
			.map((row) => ({ oldLine: row.beforeLineNumber, newLine: row.afterLineNumber }));

		if (ctrlOrMeta && start === index) {
			const line = selected[0];
			if (!line) return;
			const key = lineIdKey(line);
			lines = lines.some((line) => lineIdKey(line) === key)
				? lines.filter((line) => lineIdKey(line) !== key)
				: [...lines, line];
		} else {
			lines = selected;
		}

		if (!shift && index === startIndex) anchor = index;
	}
</script>

<div class="staging-hunk">
	{#if selectable}
		<div class="hunk-actions">
			<Button kind="outline" size="tag" disabled={busy} onclick={moveHunk}>
				{action} hunk
			</Button>
			{#if lines.length > 0}
				<Button
					kind="outline"
					size="tag"
					disabled={busy}
					onclick={() => onmove(lineIdsToHunkHeaders(lines, hunk.diff, "commit"))}
				>
					{action} selected lines ({lines.length})
				</Button>
			{/if}
		</div>
	{/if}
	<div inert={busy}>
		<HunkDiff
			{filePath}
			hunkStr={hunk.diff}
			{selectable}
			selectLinesOnContent
			staged={lines.length > 0}
			stagedLines={lines}
			hideCheckboxes
			draggingDisabled
			onLineClick={selectLines}
			{...uiState.pick(
				"diffLigatures",
				"tabSize",
				"wrapText",
				"diffFont",
				"diffFontSize",
				"strongContrast",
				"colorBlindFriendly",
				"inlineUnifiedDiffs",
			)}
		/>
	</div>
</div>

<style>
	.staging-hunk {
		padding: 0 12px 12px;
	}
	.hunk-actions {
		display: flex;
		align-items: center;
		padding: 6px 0;
		gap: 8px;
	}
</style>
