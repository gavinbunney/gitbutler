import SplitStagingView from "$components/views/SplitStagingView.svelte";
import { projectRunCommitHooks } from "$lib/config/config";
import { DROPZONE_REGISTRY, DropzoneRegistry } from "$lib/dragging/registry";
import { RESIZE_SYNC, ResizeSync } from "$lib/floating/resizeSync";
import { HOOKS_SERVICE } from "$lib/git/hooksService";
import { DIFF_SERVICE } from "$lib/hunks/diffService.svelte";
import { STACK_SERVICE } from "$lib/stacks/stackService.svelte";
import { UI_STATE } from "$lib/state/uiState.svelte";
import { WORKTREE_SERVICE } from "$lib/worktree/worktreeService.svelte";
import { HunkDiff } from "@gitbutler/ui-svelte";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/svelte";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { UnifiedDiff } from "$lib/hunks/diff";
import type { StagingStatus, TreeChange } from "@gitbutler/but-sdk";

vi.mock("@sentry/sveltekit", () => ({ captureException: vi.fn() }));

beforeEach(() => {
	vi.stubGlobal(
		"IntersectionObserver",
		class {
			constructor(private callback: IntersectionObserverCallback) {}
			observe(target: Element) {
				this.callback(
					[{ isIntersecting: true, target } as IntersectionObserverEntry],
					this as unknown as IntersectionObserver,
				);
			}
			unobserve() {}
			disconnect() {}
		},
	);
	vi.stubGlobal(
		"ResizeObserver",
		class {
			observe() {}
			unobserve() {}
			disconnect() {}
		},
	);
});

const pathBytes = Array.from(new TextEncoder().encode("file.txt"));
const saved: TreeChange = {
	path: "file.txt",
	pathBytes,
	status: {
		type: "Modification",
		subject: {
			previousState: { id: "base-blob", kind: "Blob" },
			state: { id: "saved-blob", kind: "Blob" },
			flags: null,
		},
	},
};
const later: TreeChange = {
	...saved,
	status: {
		type: "Modification",
		subject: {
			previousState: { id: "saved-blob", kind: "Blob" },
			state: { id: "0000000000000000000000000000000000000000", kind: "Blob" },
			flags: null,
		},
	},
};
const status: StagingStatus = {
	staged: [saved],
	unstaged: [later],
	ignoredChanges: [],
	indexTree: "saved-tree",
};
const diffSettings = { tabSize: 8, diffFont: "Fira Code", diffFontSize: 16, strongContrast: true };

function mount(data = status, diff: UnifiedDiff = { type: "Binary" }) {
	let draft = $state({ title: "", description: "" });
	const stageFiles = vi.fn().mockResolvedValue(undefined);
	const unstageFiles = vi.fn().mockResolvedValue(undefined);
	const commitIndex = vi.fn().mockResolvedValue({ newCommit: "new-commit" });
	const runPreCommitHooks = vi.fn().mockResolvedValue(undefined);
	const messageHook = vi.fn().mockResolvedValue({ status: "success" });
	const getDiff = vi.fn(() => ({ result: { status: "fulfilled", data: diff } }));
	const context = new Map<unknown, unknown>([
		[DROPZONE_REGISTRY._key, new DropzoneRegistry()],
		[RESIZE_SYNC._key, new ResizeSync()],
		[
			WORKTREE_SERVICE._key,
			{
				stagingStatus: () => ({ result: { status: "fulfilled", data } }),
				fetchStagingStatus: vi.fn().mockResolvedValue({ ...data, indexTree: "hook-tree" }),
				stageFiles,
				unstageFiles,
				commitIndex,
			},
		],
		[
			STACK_SERVICE._key,
			{
				stacks: () => ({
					result: {
						status: "fulfilled",
						data: [{ segments: [{ refName: { displayName: "feature" } }] }],
					},
				}),
			},
		],
		[DIFF_SERVICE._key, { getDiff }],
		[
			HOOKS_SERVICE._key,
			{
				message: [messageHook],
				runPreCommitHooks,
				runPostCommitHooks: vi.fn().mockResolvedValue(undefined),
			},
		],
		[
			UI_STATE._key,
			{
				global: { zoom: { current: 1 } },
				pick: (...keys: (keyof typeof diffSettings)[]) =>
					Object.fromEntries(keys.map((key) => [key, diffSettings[key]])),
				lane: () => ({
					newCommitMessage: {
						get current() {
							return draft;
						},
						set(value: typeof draft) {
							draft = value;
						},
					},
				}),
			},
		],
	]);
	render(SplitStagingView, { props: { projectId: "project" }, context });
	return { stageFiles, unstageFiles, commitIndex, getDiff, runPreCommitHooks };
}

afterEach(() => {
	cleanup();
	projectRunCommitHooks("project").set(false);
	vi.unstubAllGlobals();
});

test("keeps both versions of the same file visible and diffs the selected side", async () => {
	const { getDiff } = mount();
	const staged = screen.getByRole("listbox", { name: "Staged files" });
	const unstaged = screen.getByRole("listbox", { name: "Unstaged files" });
	expect(within(staged).getByRole("checkbox")).toBeChecked();
	expect(within(unstaged).getByRole("checkbox")).not.toBeChecked();
	await fireEvent.click(within(staged).getByRole("option"));
	expect(getDiff).toHaveBeenLastCalledWith("project", saved);
	await fireEvent.click(within(unstaged).getByRole("option"));
	expect(getDiff).toHaveBeenLastCalledWith("project", later);
	await fireEvent.input(screen.getByLabelText("Commit message"), {
		target: { value: "Save version" },
	});
	expect(staged).toBeVisible();
	expect(unstaged).toBeVisible();
});

test("stages and unstages whole files using their original path bytes", async () => {
	const { stageFiles, unstageFiles } = mount();
	await fireEvent.click(screen.getByRole("button", { name: "Stage all" }));
	expect(stageFiles).toHaveBeenCalledWith({
		projectId: "project",
		changes: [{ pathBytes, previousPathBytes: null, hunkHeaders: [] }],
	});
	await fireEvent.click(screen.getByRole("button", { name: "Unstage all" }));
	expect(unstageFiles).toHaveBeenCalledWith({
		projectId: "project",
		changes: [{ pathBytes, previousPathBytes: null, hunkHeaders: [] }],
	});
});

test.each([
	["Unstaged files", "Staged files", "stageFiles"],
	["Staged files", "Unstaged files", "unstageFiles"],
] as const)("drags from %s into an empty %s list", async (source, destination, mutation) => {
	const services = mount({
		...status,
		staged: source === "Staged files" ? [saved] : [],
		unstaged: source === "Unstaged files" ? [later] : [],
	});
	const sourceList = screen.getByRole("listbox", { name: source });
	const targetList = screen.getByRole("listbox", { name: destination });
	await fireEvent.mouseDown(within(sourceList).getByRole("option"), {
		button: 0,
		clientX: 20,
		clientY: 200,
	});
	await fireEvent.mouseMove(window, { clientX: 20, clientY: 30 });
	await fireEvent.mouseUp(targetList, { clientX: 20, clientY: 30 });
	expect(services[mutation]).toHaveBeenCalledWith({
		projectId: "project",
		changes: [{ pathBytes, previousPathBytes: null, hunkHeaders: [] }],
	});
	expect(
		services[mutation === "stageFiles" ? "unstageFiles" : "stageFiles"],
	).not.toHaveBeenCalled();
});

test("commits the displayed saved tree without adding the unstaged file", async () => {
	const { commitIndex, stageFiles } = mount();
	await fireEvent.input(screen.getByLabelText("Commit message"), {
		target: { value: "Save version" },
	});
	await fireEvent.click(screen.getByRole("button", { name: /Commit staged files/ }));
	expect(commitIndex).toHaveBeenCalledWith({
		projectId: "project",
		target: "refs/heads/feature",
		expectedIndexTree: "saved-tree",
		message: "Save version",
	});
	expect(stageFiles).not.toHaveBeenCalled();
	expect(screen.getByLabelText("Commit message")).toHaveValue("");
});

test("commits with the keyboard shortcut while Enter keeps editing the message", async () => {
	const { commitIndex } = mount();
	const message = screen.getByLabelText("Commit message");
	await fireEvent.keyDown(message, { key: "Enter", metaKey: true });
	expect(commitIndex).not.toHaveBeenCalled();
	await fireEvent.input(message, { target: { value: "Save version" } });
	await fireEvent.keyDown(message, { key: "Enter" });
	expect(commitIndex).not.toHaveBeenCalled();
	await fireEvent.keyDown(message, { key: "Enter", metaKey: true });
	expect(commitIndex).toHaveBeenCalledOnce();
});

test("runs hooks on the real index and commits hook-staged changes", async () => {
	projectRunCommitHooks("project").set(true);
	const { commitIndex, runPreCommitHooks } = mount();
	await fireEvent.input(screen.getByLabelText("Commit message"), {
		target: { value: "Save version" },
	});
	await fireEvent.click(screen.getByRole("button", { name: /Commit staged files/ }));
	expect(runPreCommitHooks).toHaveBeenCalledWith("project", "index");
	expect(commitIndex).toHaveBeenCalledWith(
		expect.objectContaining({ expectedIndexTree: "hook-tree" }),
	);
});

test("disables commit while the index has unresolved conflicts", () => {
	mount({
		...status,
		indexTree: null,
		ignoredChanges: [{ path: "conflict.txt", status: "Conflict" }],
	});
	expect(screen.getByRole("button", { name: /Commit staged files/ })).toBeDisabled();
	expect(screen.getByText(/Resolve these files before committing/)).toHaveTextContent(
		"conflict.txt",
	);
});

const textDiff: UnifiedDiff = {
	type: "Patch",
	subject: {
		hunks: [
			{
				oldStart: 1,
				oldLines: 3,
				newStart: 1,
				newLines: 3,
				diff: "@@ -1,3 +1,3 @@\n-one\n-two\n+ONE\n+TWO\n three\n",
			},
		],
		linesAdded: 2,
		linesRemoved: 2,
		isResultOfBinaryToTextConversion: false,
	},
};

test.each([false, true])("uses the shared diff preferences (textconv=%s)", async (textconv) => {
	if (textDiff.type !== "Patch") throw new Error("expected a text patch");
	mount(status, {
		...textDiff,
		subject: { ...textDiff.subject, isResultOfBinaryToTextConversion: textconv },
	});
	await fireEvent.click(
		within(screen.getByRole("listbox", { name: "Unstaged files" })).getByRole("option"),
	);
	const hunk = (await screen.findByText("ONE")).closest(".table__wrapper");
	expect(hunk).toHaveStyle({
		"--tab-size": "8",
		"--diff-font": "Fira Code",
		"--diff-font-size": "16px",
	});
	expect(hunk).toHaveClass("contrast-strong");
});

test.each([
	["Unstaged files", "Stage", "stageFiles"],
	["Staged files", "Unstage", "unstageFiles"],
] as const)("moves a single hunk from %s", async (side, action, mutation) => {
	const services = mount(status, textDiff);
	await fireEvent.click(within(screen.getByRole("listbox", { name: side })).getByRole("option"));
	await fireEvent.click(screen.getByRole("button", { name: `${action} hunk` }));
	expect(services[mutation]).toHaveBeenCalledWith({
		projectId: "project",
		changes: [
			{
				pathBytes,
				previousPathBytes: null,
				hunkHeaders: [{ oldStart: 1, oldLines: 3, newStart: 1, newLines: 3 }],
			},
		],
	});
});

test.each([
	["Unstaged files", "Stage", "stageFiles"],
	["Staged files", "Unstage", "unstageFiles"],
] as const)("selects a whole line through its text in %s", async (side, action, mutation) => {
	const services = mount(status, textDiff);
	await fireEvent.click(within(screen.getByRole("listbox", { name: side })).getByRole("option"));
	const text = await screen.findByText("ONE");
	const down = new MouseEvent("mousedown", { button: 0, bubbles: true, cancelable: true });
	await fireEvent(text, down);
	await fireEvent.mouseUp(text);
	expect(down.defaultPrevented).toBe(true);
	expect(text.closest("td")).toHaveClass("staged");
	await fireEvent.click(screen.getByRole("button", { name: `${action} selected lines (1)` }));
	expect(services[mutation]).toHaveBeenCalledWith({
		projectId: "project",
		changes: [
			{
				pathBytes,
				previousPathBytes: null,
				hunkHeaders: [{ oldStart: 0, oldLines: 0, newStart: 1, newLines: 1 }],
			},
		],
	});
});

test("keeps ordinary diff text selectable outside the staging view", async () => {
	if (textDiff.type !== "Patch") throw new Error("expected a text patch");
	const onLineClick = vi.fn();
	render(HunkDiff, {
		props: {
			filePath: "file.txt",
			hunkStr: textDiff.subject.hunks[0]!.diff,
			selectable: true,
			onLineClick,
		},
	});
	const text = await screen.findByText("ONE");
	const down = new MouseEvent("mousedown", { button: 0, bubbles: true, cancelable: true });
	await fireEvent(text, down);
	await fireEvent.mouseUp(text);
	expect(down.defaultPrevented).toBe(false);
	expect(onLineClick).not.toHaveBeenCalled();
	await fireEvent.mouseDown(within(text.closest("tr")!).getAllByTestId("hunk-count-column")[1]!);
	expect(onLineClick).toHaveBeenCalledOnce();
});

test.each([
	["Unstaged files", "Stage", "stageFiles", 1],
	["Staged files", "Unstage", "unstageFiles", 1],
	["Unstaged files", "Stage", "stageFiles", 0],
	["Staged files", "Unstage", "unstageFiles", 0],
] as const)(
	"moves only selected lines from %s with %s (%s, buttons=%s)",
	async (side, action, mutation, buttons) => {
		const services = mount(status, textDiff);
		await fireEvent.click(within(screen.getByRole("listbox", { name: side })).getByRole("option"));
		const row = (await screen.findByText("ONE")).closest("tr")!;
		const gutter = within(row).getAllByTestId("hunk-count-column")[1]!;
		await fireEvent.mouseDown(gutter, { button: 0, buttons });
		await fireEvent.mouseUp(gutter);
		expect(services[mutation]).not.toHaveBeenCalled();
		await fireEvent.click(screen.getByRole("button", { name: `${action} selected lines (1)` }));
		expect(services[mutation]).toHaveBeenCalledWith({
			projectId: "project",
			changes: [
				{
					pathBytes,
					previousPathBytes: null,
					hunkHeaders: [{ oldStart: 0, oldLines: 0, newStart: 1, newLines: 1 }],
				},
			],
		});
	},
);

test.each(["ctrlKey", "metaKey"] as const)(
	"extends a line selection with Shift and toggles a separate line with %s",
	async (modifier) => {
		const { stageFiles } = mount(status, textDiff);
		await fireEvent.click(
			within(screen.getByRole("listbox", { name: "Unstaged files" })).getByRole("option"),
		);
		async function gutter(text: string) {
			return within((await screen.findByText(text)).closest("tr")!).getAllByTestId(
				"hunk-count-column",
			)[1]!;
		}

		const first = await screen.findByText("one");
		await fireEvent.mouseDown(first, { button: 0, buttons: 1 });
		await fireEvent.mouseUp(first);
		const last = await screen.findByText("ONE");
		await fireEvent.mouseDown(last, { button: 0, buttons: 1, shiftKey: true });
		await fireEvent.mouseUp(last);
		expect(screen.getByRole("button", { name: "Stage selected lines (3)" })).toBeInTheDocument();

		const middle = await screen.findByText("two");
		await fireEvent.mouseDown(middle, { button: 0, buttons: 1, [modifier]: true });
		await fireEvent.mouseEnter(await gutter("two"), { buttons: 1, [modifier]: true });
		await fireEvent.mouseEnter(middle.closest("td")!, { buttons: 1, [modifier]: true });
		await fireEvent.mouseUp(middle);
		await fireEvent.click(screen.getByRole("button", { name: "Stage selected lines (2)" }));
		expect(stageFiles).toHaveBeenCalledWith({
			projectId: "project",
			changes: [
				{
					pathBytes,
					previousPathBytes: null,
					hunkHeaders: [
						{ oldStart: 1, oldLines: 1, newStart: 0, newLines: 0 },
						{ oldStart: 0, oldLines: 0, newStart: 1, newLines: 1 },
					],
				},
			],
		});
	},
);

test.each([
	[0, "gutter"],
	[1, "gutter"],
	[0, "text"],
	[1, "text"],
] as const)(
	"selects a dragged line range across diff rendering chunks with buttons=%s through %s",
	async (buttons, surface) => {
		const diff: UnifiedDiff = {
			type: "Patch",
			subject: {
				hunks: [
					{
						oldStart: 1,
						oldLines: 1,
						newStart: 1,
						newLines: 16,
						diff: `@@ -1,1 +1,16 @@\n context\n${Array.from({ length: 15 }, (_, index) => `+added${index + 1}\n`).join("")}`,
					},
				],
				linesAdded: 15,
				linesRemoved: 0,
				isResultOfBinaryToTextConversion: false,
			},
		};
		const { stageFiles } = mount(status, diff);
		await fireEvent.click(
			within(screen.getByRole("listbox", { name: "Unstaged files" })).getByRole("option"),
		);
		const first = await screen.findByText("added1");
		const last = screen.getByText("added12");
		const start =
			surface === "text"
				? first
				: within(first.closest("tr")!).getAllByTestId("hunk-count-column")[1]!;
		const end =
			surface === "text"
				? last.closest("td")!
				: within(last.closest("tr")!).getAllByTestId("hunk-count-column")[1]!;
		await fireEvent.mouseDown(start, { button: 0, buttons: 1 });
		await fireEvent.mouseEnter(end, { buttons });
		await fireEvent.mouseUp(end);
		await fireEvent.click(screen.getByRole("button", { name: "Stage selected lines (12)" }));
		expect(stageFiles).toHaveBeenCalledWith({
			projectId: "project",
			changes: [
				{
					pathBytes,
					previousPathBytes: null,
					hunkHeaders: [{ oldStart: 0, oldLines: 0, newStart: 2, newLines: 12 }],
				},
			],
		});
	},
);

test("ends line dragging when the mouse is released outside the diff", async () => {
	mount(status, textDiff);
	await fireEvent.click(
		within(screen.getByRole("listbox", { name: "Unstaged files" })).getByRole("option"),
	);
	const first = within((await screen.findByText("ONE")).closest("tr")!).getAllByTestId(
		"hunk-count-column",
	)[1]!;
	const last = within(screen.getByText("TWO").closest("tr")!).getAllByTestId(
		"hunk-count-column",
	)[1]!;
	await fireEvent.mouseDown(first, { button: 0, buttons: 1 });
	await fireEvent.mouseUp(window);
	await fireEvent.mouseEnter(last, { buttons: 1 });
	expect(screen.getByRole("button", { name: "Stage selected lines (1)" })).toBeInTheDocument();
});

test("keeps textconv previews read-only for partial staging", async () => {
	if (textDiff.type !== "Patch") throw new Error("expected a text patch");
	mount(status, {
		...textDiff,
		subject: { ...textDiff.subject, isResultOfBinaryToTextConversion: true },
	});
	await fireEvent.click(
		within(screen.getByRole("listbox", { name: "Unstaged files" })).getByRole("option"),
	);
	const text = await screen.findByText("ONE");
	const down = new MouseEvent("mousedown", { button: 0, bubbles: true, cancelable: true });
	await fireEvent(text, down);
	await fireEvent.mouseUp(text);
	expect(down.defaultPrevented).toBe(false);
	expect(screen.queryByRole("button", { name: "Stage hunk" })).not.toBeInTheDocument();
});
