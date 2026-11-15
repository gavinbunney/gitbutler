import Resizer from "$components/shared/Resizer.svelte";
import { RESIZE_SYNC, ResizeSync } from "$lib/floating/resizeSync";
import { SASH_LAYER, type SashLayerContext } from "$lib/sash/sashLayer";
import { UI_STATE } from "$lib/state/uiState.svelte";
import { fireEvent, render, waitFor } from "@testing-library/svelte";
import { describe, expect, test, vi } from "vitest";

function mockUiState() {
	return {
		global: {
			zoom: { current: 1, set: vi.fn() },
		},
	};
}

function baseContext(): Map<any, any> {
	return new Map<any, any>([
		[UI_STATE._key, mockUiState()],
		[RESIZE_SYNC._key, new ResizeSync()],
	]);
}

function baseProps() {
	const viewport = document.createElement("div");
	viewport.style.width = "300px";
	viewport.style.height = "200px";
	return {
		defaultValue: 20,
		viewport,
		direction: "right" as const,
	};
}

describe("Resizer", () => {
	test("throws when mounted without SashLayer context", () => {
		expect(() => {
			render(Resizer, {
				props: baseProps(),
				context: baseContext(),
			});
		}).toThrow("Resizer must be used inside <SashLayer>.");
	});

	test("mounts when SashLayer context is provided", async () => {
		const container = document.createElement("div");
		document.body.appendChild(container);
		const layerCtx: SashLayerContext = {
			container,
			requestLayout: vi.fn(),
			subscribeLayout: () => () => {},
			observeLayoutTarget: () => () => {},
			setAutoLayoutPaused: vi.fn(),
		};

		const context = baseContext();
		context.set(SASH_LAYER, layerCtx);

		const { unmount } = render(Resizer, {
			props: baseProps(),
			context,
		});

		await waitFor(() => expect(container.querySelector(".resizer")).toBeInTheDocument());
		unmount();
		container.remove();
	});

	test.each(["pointerup", "pointercancel", "lostpointercapture", "blur"] as const)(
		"ends resizing on %s without requiring another click",
		async (event) => {
			const container = document.createElement("div");
			document.body.appendChild(container);
			const layerCtx: SashLayerContext = {
				container,
				requestLayout: vi.fn(),
				subscribeLayout: () => () => {},
				observeLayoutTarget: () => () => {},
				setAutoLayoutPaused: vi.fn(),
			};
			const context = baseContext();
			context.set(SASH_LAYER, layerCtx);
			const onResizing = vi.fn();
			const { unmount } = render(Resizer, {
				props: { ...baseProps(), onResizing },
				context,
			});

			try {
				const resizer = container.querySelector<HTMLDivElement>(".resizer")!;
				resizer.setPointerCapture = vi.fn();
				resizer.hasPointerCapture = vi.fn().mockReturnValue(true);
				resizer.releasePointerCapture = vi.fn();
				const down = new MouseEvent("pointerdown", {
					button: 0,
					clientX: 300,
					bubbles: true,
				});
				Object.defineProperty(down, "pointerId", { value: 1 });
				await fireEvent(resizer, down);
				await fireEvent(resizer, new MouseEvent("pointermove", { clientX: 350, bubbles: true }));
				expect(onResizing).toHaveBeenLastCalledWith(true);
				expect(resizer).toHaveClass("is-resizing");

				await fireEvent(event === "blur" ? window : resizer, new Event(event, { bubbles: true }));
				expect(onResizing).toHaveBeenLastCalledWith(false);
				expect(resizer).not.toHaveClass("is-resizing");
				expect(layerCtx.setAutoLayoutPaused).toHaveBeenLastCalledWith(false);
				expect(resizer.releasePointerCapture).toHaveBeenCalledWith(1);

				const completedCalls = onResizing.mock.calls.length;
				await fireEvent.pointerMove(resizer, { clientX: 400 });
				expect(resizer).not.toHaveClass("is-resizing");
				expect(onResizing).toHaveBeenCalledTimes(completedCalls);
			} finally {
				unmount();
				container.remove();
			}
		},
	);
});
