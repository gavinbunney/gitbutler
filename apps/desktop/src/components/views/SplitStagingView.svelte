<script lang="ts">
	import StagingCommitForm from "$components/commit/StagingCommitForm.svelte";
	import StagingDiff from "$components/diff/StagingDiff.svelte";
	import StagingFileList from "$components/files/StagingFileList.svelte";
	import ReduxResult from "$components/shared/ReduxResult.svelte";
	import Resizer from "$components/shared/Resizer.svelte";
	import SashLayer from "$components/shared/SashLayer.svelte";
	import { changesToDiffSpec } from "$lib/commits/utils";
	import { UI_STATE } from "$lib/state/uiState.svelte";
	import { WORKTREE_SERVICE } from "$lib/worktree/worktreeService.svelte";
	import { inject } from "@gitbutler/core/context";
	import { persisted } from "@gitbutler/shared/persisted";
	import { pxToRem } from "@gitbutler/ui-svelte/utils/pxToRem";
	import type { HunkHeader, TreeChange } from "@gitbutler/but-sdk";

	type Props = { projectId: string };
	const { projectId }: Props = $props();
	const worktreeService = inject(WORKTREE_SERVICE);
	const uiState = inject(UI_STATE);
	const zoom = $derived(uiState.global.zoom.current);
	const statusQuery = $derived(worktreeService.stagingStatus(projectId));
	const status = $derived(statusQuery.result?.data);
	const split = persisted(50, "split-staging-list-height");
	let selected = $state<{ side: "staged" | "unstaged"; path: string }>();
	let busy = $state(false);
	let lists = $state<HTMLDivElement>();
	let containerHeight = $state(600);
	let contentWidth = $state(1000);
	let contentHeight = $state(400);
	const maxCommitHeight = $derived(Math.max(10, pxToRem(containerHeight - 240, zoom)));
	const maxListWidth = $derived(Math.max(15, pxToRem(contentWidth - 240, zoom)));
	const minListSplit = $derived(
		Math.min(50, Math.max(20, (100 / Math.max(1, contentHeight - 6)) * 100)),
	);
	const maxListSplit = $derived(100 - minListSplit);
	const listSplit = $derived(Math.max(minListSplit, Math.min(maxListSplit, $split)));

	const selectedChange = $derived(
		selected &&
			status?.[selected.side].find((change) => change.pathBytes.join(",") === selected?.path),
	);

	async function moveFiles(
		changes: TreeChange[],
		side: "staged" | "unstaged",
		headers?: HunkHeader[],
	) {
		if (busy || changes.length === 0) return;
		if (headers?.length === 0) return;
		busy = true;

		try {
			const args = {
				projectId,
				changes: changesToDiffSpec(changes).map((spec) => ({
					...spec,
					hunkHeaders: headers ?? spec.hunkHeaders,
				})),
			};

			if (side === "staged") {
				await worktreeService.unstageFiles(args);
			} else {
				await worktreeService.stageFiles(args);
			}

			if (
				!headers &&
				selected?.side === side &&
				changes.some((change) => change.pathBytes.join(",") === selected?.path)
			) {
				selected = { ...selected, side: side === "staged" ? "unstaged" : "staged" };
			}
		} finally {
			busy = false;
		}
	}

	function resize(event: PointerEvent) {
		const rect = lists?.getBoundingClientRect();
		if (!rect) return;
		split.set(
			Math.max(
				minListSplit,
				Math.min(maxListSplit, ((event.clientY - rect.top - 3) / (rect.height - 6)) * 100),
			),
		);
	}
</script>

<SashLayer>
	<ReduxResult {projectId} result={statusQuery.result}>
		{#snippet children(data)}
			<div class="split-staging" bind:clientHeight={containerHeight}>
				<div
					class="files-and-diff"
					bind:clientWidth={contentWidth}
					bind:clientHeight={contentHeight}
				>
					<div
						class="lists"
						bind:this={lists}
						style:grid-template-rows={`minmax(0, ${listSplit}fr) 6px minmax(0, ${100 - listSplit}fr)`}
					>
						<Resizer
							viewport={lists}
							direction="right"
							defaultValue={20}
							minWidth={15}
							maxWidth={maxListWidth}
							persistId="split-staging-list-width"
						/>
						<StagingFileList
							{projectId}
							changes={data.staged}
							side="staged"
							selectedPath={selected?.side === "staged" ? selected.path : undefined}
							{busy}
							onselect={(change) => {
								selected = { side: "staged", path: change.pathBytes.join(",") };
							}}
							onmove={moveFiles}
						/>
						<!-- a focusable separator implements the WAI-ARIA window splitter pattern -->
						<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
						<div
							class="divider"
							role="separator"
							aria-label="Resize staged and unstaged lists"
							aria-orientation="horizontal"
							aria-valuenow={listSplit}
							aria-valuemin={minListSplit}
							aria-valuemax={maxListSplit}
							tabindex="0"
							onpointerdown={(event) => {
								event.preventDefault();
								event.currentTarget.setPointerCapture(event.pointerId);
							}}
							onpointermove={(event) => {
								if (event.currentTarget.hasPointerCapture(event.pointerId)) resize(event);
							}}
							ondblclick={() => split.set(50)}
							onkeydown={(event) => {
								if (event.key === "ArrowUp" || event.key === "ArrowDown") {
									event.preventDefault();
									split.set(
										Math.max(
											minListSplit,
											Math.min(maxListSplit, listSplit + (event.key === "ArrowUp" ? -5 : 5)),
										),
									);
								}
							}}
						></div>
						<StagingFileList
							{projectId}
							changes={data.unstaged}
							side="unstaged"
							selectedPath={selected?.side === "unstaged" ? selected.path : undefined}
							{busy}
							onselect={(change) => {
								selected = { side: "unstaged", path: change.pathBytes.join(",") };
							}}
							onmove={moveFiles}
						/>
					</div>
					<div class="diff">
						{#if selected && selectedChange}
							{@const side = selected.side}
							{@const change = selectedChange}
							<StagingDiff
								{projectId}
								{change}
								{side}
								{busy}
								onmove={(headers) => moveFiles([change], side, headers)}
							/>
						{:else}
							<p class="empty">Select a file to review its changes</p>
						{/if}
					</div>
				</div>
				{#if data.ignoredChanges.length > 0}
					<p class="notice">
						Resolve these files before committing: {data.ignoredChanges
							.map((change) => change.path)
							.join(", ")}
					</p>
				{/if}
				<StagingCommitForm {projectId} status={data} bind:busy maxHeight={maxCommitHeight} />
			</div>
		{/snippet}
	</ReduxResult>
</SashLayer>

<style>
	.split-staging {
		display: flex;
		flex: 1;
		flex-direction: column;
		min-width: 0;
		height: 100%;
		min-height: 0;
		overflow: hidden;
		border: 1px solid var(--border-2);
		border-radius: var(--radius-ml);
		background: var(--bg-1);
	}
	.files-and-diff {
		display: flex;
		flex: 1;
		min-height: 0;
	}
	.lists {
		display: grid;
		position: relative;
		flex-shrink: 0;
		min-width: 0;
		min-height: 0;
		border-right: 1px solid var(--border-2);
	}
	.divider {
		position: relative;
		border-top: 1px solid var(--border-2);
		border-bottom: 1px solid var(--border-2);
		background: var(--bg-2);
		cursor: row-resize;
		touch-action: none;
	}
	.divider::after {
		position: absolute;
		top: 1px;
		left: calc(50% - 12px);
		width: 24px;
		height: 2px;
		border-radius: 2px;
		background: var(--border-2);
		content: "";
	}
	.diff {
		flex: 1;
		min-width: 0;
		overflow: auto;
	}
	.divider:focus-visible {
		outline: 2px solid var(--text-1);
	}
	.empty {
		padding: 16px;
		color: var(--text-2);
		font-size: 12px;
	}
	.notice {
		padding: 8px 16px;
		color: var(--text-2);
		font-size: 12px;
	}
</style>
