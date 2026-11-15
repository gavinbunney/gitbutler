<script lang="ts">
	import Dropzone from "$components/shared/Dropzone.svelte";
	import { draggableChips } from "$lib/dragging/draggable";
	import { StagingFileDropData } from "$lib/dragging/draggables";
	import { DROPZONE_REGISTRY } from "$lib/dragging/registry";
	import { computeChangeStatus } from "$lib/files/fileStatus";
	import { inject } from "@gitbutler/core/context";
	import { Button, FileListItem } from "@gitbutler/ui-svelte";
	import type { TreeChange } from "@gitbutler/but-sdk";

	type Props = {
		projectId: string;
		changes: TreeChange[];
		side: "staged" | "unstaged";
		selectedPath?: string;
		busy: boolean;
		onselect: (change: TreeChange) => void;
		onmove: (changes: TreeChange[], side: "staged" | "unstaged") => Promise<void>;
	};
	const { projectId, changes, side, selectedPath, busy, onselect, onmove }: Props = $props();
	const dropzoneRegistry = inject(DROPZONE_REGISTRY);
	let hovered = $state(false);
</script>

<Dropzone
	overflow
	disabled={busy}
	onHovered={(value) => (hovered = value)}
	handlers={[
		{
			accepts: (data: unknown) =>
				!busy &&
				data instanceof StagingFileDropData &&
				data.projectId === projectId &&
				data.side !== side,
			ondrop: (data: StagingFileDropData) => onmove([data.change], data.side),
		},
	]}
>
	<section
		class="file-section"
		class:drop-hover={hovered}
		aria-label={side === "staged" ? "Staged files" : "Unstaged files"}
	>
		<header>
			<strong>{side === "staged" ? "Staged files" : "Unstaged files"} ({changes.length})</strong>
			<Button
				kind="outline"
				size="tag"
				disabled={busy || changes.length === 0}
				onclick={() => onmove(changes, side)}
			>
				{side === "staged" ? "Unstage all" : "Stage all"}
			</Button>
		</header>
		<p class="drag-hint">
			{side === "staged" ? "Drag files here to stage" : "Drag files here to unstage"}
		</p>
		<div
			class="file-list"
			role="listbox"
			aria-label={side === "staged" ? "Staged files" : "Unstaged files"}
			aria-busy={busy}
			inert={busy}
		>
			{#each changes as change (change.pathBytes.join(","))}
				<div
					class="file-row"
					title={side === "staged"
						? "Drag to Unstaged files to unstage"
						: "Drag to Staged files to stage"}
					use:draggableChips={{
						filePath: change.path,
						label: change.path,
						chipType: "file",
						data: new StagingFileDropData(projectId, side, change),
						disabled: busy,
						dropzoneRegistry,
					}}
				>
					<FileListItem
						filePath={change.path}
						fileStatus={computeChangeStatus(change)}
						showCheckbox
						draggable={!busy}
						checked={side === "staged"}
						selected={selectedPath === change.pathBytes.join(",")}
						onclick={() => onselect(change)}
						oncheck={() => onmove([change], side)}
						ondblclick={() => onmove([change], side)}
						onkeydown={(event) => {
							if (event.key === "Enter") onselect(change);
						}}
					/>
				</div>
			{:else}
				<p class="empty">{side === "staged" ? "No staged files" : "No unstaged files"}</p>
			{/each}
		</div>
	</section>
</Dropzone>

<style>
	.file-section {
		display: flex;
		flex: 1;
		flex-direction: column;
		min-height: 0;
		overflow: hidden;
	}
	.file-section.drop-hover {
		outline: 2px solid var(--btn-pop-outline);
		outline-offset: -2px;
		background: var(--focus-bg-mute);
	}
	.drag-hint {
		flex-shrink: 0;
		margin: 0;
		padding: 6px 12px;
		color: var(--text-2);
		font-size: 11px;
	}
	.file-list {
		flex: 1;
		min-height: 0;
		overflow: auto;
	}
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
	.empty {
		padding: 16px;
		color: var(--text-2);
		font-size: 12px;
	}
</style>
