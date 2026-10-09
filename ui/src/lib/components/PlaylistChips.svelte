<script lang="ts">
	// The playlists holding a track, as chips: two and "+N" at rest (PlaylistForge's rule: enough to
	// recognise, never a wall). Hovering or focusing the zone lifts it into a highlighted overlay that
	// reveals up to five; past five the chips loop through that window (see chips.ts). A chip opens
	// its playlist, Alt/Shift+click hands it to `onchipalt`, and "+N" calls `onmore`.
	import { CAROUSEL_DELAY_MS, chipView, loopDuration } from '$lib/chips';
	import { t } from '$lib/i18n.svelte';
	import { Badge } from './ui/badge';

	let {
		ids,
		nameOf,
		initial = 2,
		max = 5,
		onmore,
		onchipalt
	}: {
		/** Playlist ids, in the order the chips read. */
		ids: string[];
		nameOf: (id: string) => string;
		/** Chips at rest. */
		initial?: number;
		/** Chips the revealed window holds before it loops. */
		max?: number;
		/** "+N": every playlist at once (the occurrences dialog). */
		onmore?: () => void;
		/** Alt/Shift+click on a chip, instead of opening the playlist. */
		onchipalt?: (id: string) => void;
	} = $props();

	/** `gap-1` between chips, in px: the loop measures with it. */
	const GAP = 4;

	let hovered = $state(false);
	let focused = $state(false);
	let still = $state(false);
	const revealed = $derived(hovered || focused);
	const view = $derived(chipView(ids.length, revealed, still, initial, max));
	// Kept whenever anything is held back at rest, so the pointer that reached for "+N" (and the
	// keyboard that tabbed to it) still finds it once the zone opens; with nothing left to reveal it
	// reads "…" and still opens the full list.
	const hasButton = $derived(ids.length > Math.max(0, initial));

	let track = $state<HTMLElement>();
	/** Width of the window that shows `max` chips, and of one full pass of the loop, in px. */
	let windowPx = $state(0);
	let passPx = $state(0);

	function reveal() {
		// Asked on every reveal rather than once: the OS setting can change while the app runs.
		still = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
	}

	// Measure the loop once the copy is drawn: the window ends where chip `max` does, and one pass
	// is the chips' own width (their copy starts one gap after it). Offsets are taken from the first
	// chip, so the overlay's padding stays out of both; a running transform does not move them.
	$effect(() => {
		ids; // remeasure when the playlists change
		const el = track;
		if (!el || !view.carousel) {
			windowPx = passPx = 0;
			return;
		}
		const chips = el.querySelectorAll<HTMLElement>('[data-chip]');
		const first = chips[0];
		const last = chips[Math.max(initial, max) - 1];
		const copy = el.querySelector<HTMLElement>('[data-chip-copy]');
		if (!first || !last || !copy) return;
		windowPx = last.offsetLeft + last.offsetWidth - first.offsetLeft;
		passPx = copy.offsetLeft - first.offsetLeft - GAP;
	});

	function open(e: MouseEvent, id: string) {
		if (!onchipalt || !(e.altKey || e.shiftKey)) return;
		e.preventDefault();
		onchipalt(id);
	}

	function focusOut(e: FocusEvent) {
		const zone = e.currentTarget as HTMLElement;
		if (!zone.contains(e.relatedTarget as Node | null)) focused = false;
	}
</script>

{#snippet chip(id: string, copy: boolean, first = false)}
	<a
		href="/playlist/{encodeURIComponent(id)}"
		class="max-w-36"
		data-chip={copy ? undefined : ''}
		data-chip-copy={copy && first ? '' : undefined}
		aria-hidden={copy ? 'true' : undefined}
		tabindex={copy ? -1 : undefined}
		onclick={(e) => open(e, id)}
	>
		<Badge variant="muted" class="max-w-36"><span class="truncate">{nameOf(id)}</span></Badge>
	</a>
{/snippet}

{#snippet more(n: number)}
	<Badge variant="outline">{n > 0 ? `+${n}` : '…'}</Badge>
{/snippet}

<span
	class="relative hidden shrink-0 items-center sm:flex"
	role="group"
	onpointerenter={() => (reveal(), (hovered = true))}
	onpointerleave={() => (hovered = false)}
	onfocusin={() => (reveal(), (focused = true))}
	onfocusout={focusOut}
>
	<!-- The resting row's footprint, never shown: the live chips sit out of flow on top of it, so
	     opening them over the title is a repaint, not a relayout of a `content-visibility` row. -->
	<span class="invisible flex items-center gap-1" aria-hidden="true" inert>
		{#each ids.slice(0, Math.max(0, initial)) as id (id)}
			<Badge variant="muted" class="max-w-36"><span class="truncate">{nameOf(id)}</span></Badge>
		{/each}
		{#if hasButton}{@render more(ids.length - initial)}{/if}
	</span>
	<!-- One tree for both states (chips keyed, the track always there), so the chip that took the
	     focus is the same node once the zone opens and keeps it. `w-max` lets it grow leftward; the
	     negative margin cancels the highlight's padding on the right, so "+N" stays under the pointer. -->
	<span
		class="absolute right-0 top-1/2 flex w-max -translate-y-1/2 items-center gap-1 rounded-full {revealed
			? 'z-10 -mr-0.5 bg-card p-0.5 ring-1 ring-primary/50'
			: ''}"
	>
		<span class="overflow-hidden" style={view.carousel && windowPx ? `width:${windowPx}px` : undefined}>
			<span
				bind:this={track}
				class="flex w-max items-center gap-1 {view.carousel && passPx ? 'chip-carousel' : ''}"
				style={view.carousel && passPx
					? `--marquee-dx:${passPx + GAP}px;animation-duration:${loopDuration(passPx, GAP).toFixed(1)}s;animation-delay:${CAROUSEL_DELAY_MS}ms`
					: undefined}
			>
				{#each ids.slice(0, view.shown) as id (id)}
					{@render chip(id, false)}
				{/each}
				<!-- The copy the loop scrolls into view (as in Marquee.svelte): when the first chip has
				     travelled one pass, its copy sits where it started. Hidden from assistive tech and the
				     tab order; a pointer can still click it, it is the same playlist. -->
				{#if view.carousel}
					{#each ids as id, i (id)}
						{@render chip(id, true, i === 0)}
					{/each}
				{/if}
			</span>
		</span>
		{#if hasButton}
			<button
				type="button"
				class="shrink-0 rounded-full"
				aria-label={view.more > 0 ? undefined : t('common.more')}
				onclick={() => onmore?.()}
			>
				{@render more(view.more)}
			</button>
		{/if}
	</span>
</span>
