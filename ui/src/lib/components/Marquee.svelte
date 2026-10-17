<script lang="ts">
	import type { Snippet } from 'svelte';
	import { MARQUEE_GAP_PX as GAP, type MarqueeMode, marqueeDuration, marqueePhase } from '$lib/marquee';

	let {
		text,
		children,
		mode = 'auto',
		class: cls = ''
	}: {
		/** The line to scroll, and the value that restarts the scroll when it changes. */
		text: string;
		/** Renders in place of plain `text` (artist links); `text` still drives the restart. */
		children?: Snippet;
		/** When an overflowing line moves (see marquee.ts). The caller reads it from `appearance`. */
		mode?: MarqueeMode;
		class?: string;
	} = $props();

	let box = $state<HTMLElement>();
	/** Full text width in px once it is wider than the box; 0 when it fits and nothing scrolls. */
	let width = $state(0);
	let reducedMotion = $state(false);
	let hovering = $state(false);
	/** The text whose three passes are over (ran out, or the pointer cut them short). */
	let introDoneFor = $state<string | null>(null);

	const phase = $derived(
		marqueePhase({
			mode,
			overflow: width > 0,
			reducedMotion,
			hovering,
			introDone: introDoneFor === text
		})
	);
	// `auto` keeps the track mounted at rest, so a line that ran its passes stops cut at its start
	// rather than at an ellipsis. Under reduced motion it never ran any, so it rests on the
	// truncated line like `hover` and `off` do.
	const track = $derived(width > 0 && ((mode === 'auto' && !reducedMotion) || phase !== 'still'));

	$effect(() => {
		text; // remeasure whenever the line changes
		const el = box;
		if (!el) return;
		const measure = () => {
			const span = el.querySelector('[data-mq]');
			// Asked on every measure rather than once: the OS setting can change while the app runs.
			// It only stops the line moving on its own (marqueePhase), never the measuring.
			reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
			const w = span ? span.scrollWidth : 0;
			width = w > el.clientWidth + 1 ? w : 0;
		};
		measure();
		// The player bar's title column stretches with the window, so a resize can hide or
		// reveal the overflow on its own.
		const ro = new ResizeObserver(measure);
		ro.observe(el);
		return () => ro.disconnect();
	});

	function leave() {
		hovering = false;
		// Back to the start, not into whatever passes were left.
		introDoneFor = text;
	}
</script>

{#snippet body()}{#if children}{@render children()}{:else}{text}{/if}{/snippet}

<!-- The hover is read off the clipping box, not the scrolling track: the trailing copy is inert, so
     it is not hit-testable and hovering it would never reach the track itself. A pointer-only
     reveal, not a control: the box itself has nothing to activate, the names inside do. -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
	bind:this={box}
	class="marquee-box overflow-hidden {cls}"
	title={mode === 'off' && width ? text : undefined}
	onpointerenter={() => (hovering = true)}
	onpointerleave={leave}
>
	{#key text}
		{#if track}
			<!-- The copy is what the loop scrolls into view: by the time the first one has moved
			     its own width plus the gap, the second sits exactly where it started, so the
			     restart is invisible and the line only ever travels one way. `inert` keeps any
			     links in the copy out of the tab order and unclickable. The gap is a margin, not
			     padding, so a hover underline drawn on this span stops at the text.
			     It is positioned, not laid out, on purpose. The title and the artist line share one
			     shrink-to-fit column, so a second in-flow copy would double this line's max-content
			     width, widen the column, and un-truncate the very text that asked to scroll: both
			     lines then flip between scrolling and still, resizing each other as they go. Out of
			     flow, scrolling and truncating measure exactly the same. -->
			<div
				class="marquee relative w-max"
				class:intro={phase === 'intro'}
				class:looping={phase === 'loop'}
				style="--marquee-dx:{width + GAP}px;animation-duration:{marqueeDuration(width).toFixed(1)}s"
				onanimationend={(e) => {
					if (e.target === e.currentTarget) introDoneFor = text;
				}}
			>
				<span class="block whitespace-nowrap" data-mq>{@render body()}</span>
				<span
					class="absolute left-full top-0 whitespace-nowrap"
					style="margin-left:{GAP}px"
					aria-hidden="true"
					inert>{@render body()}</span
				>
			</div>
		{:else}
			<span class="block truncate" data-mq>{@render body()}</span>
		{/if}
	{/key}
</div>
