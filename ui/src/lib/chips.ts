// The playlist chips on a track row (Library ▸ In your playlists): a couple at rest, more on hover
// or focus, and a looping carousel when even the revealed window cannot hold them all. Plain
// functions, no runes, so chips.check.ts can run them under Node; PlaylistChips.svelte draws them.

/** Carousel speed in px per second: slow enough to read a playlist name as it passes. */
export const CHIP_SPEED_PX_S = 30;
/** How long the revealed chips sit still before the carousel starts, so the first ones can be read. */
export const CAROUSEL_DELAY_MS = 1000;

export interface ChipView {
	/** How many chips to draw, in order from the first (the carousel draws them all). */
	shown: number;
	/** The count on the "+N" button: chips that never fit on screen at once. 0 hides it. */
	more: number;
	/** Whether the chips loop sideways instead of sitting still. */
	carousel: boolean;
}

/**
 * What a row of `total` chips shows. At rest, `initial` chips and "+N". Revealed (hovered or
 * focused), up to `max`; past that the chips loop through a `max`-wide window, unless the OS asks
 * for less motion, in which case the first `max` sit still beside "+N".
 */
export function chipView(
	total: number,
	revealed: boolean,
	reducedMotion: boolean,
	initial = 2,
	max = 5
): ChipView {
	const n = Math.max(0, Math.floor(total));
	const rest = Math.max(0, Math.floor(initial));
	// A window smaller than the resting row would hide chips on hover, which is backwards.
	const cap = Math.max(rest, Math.floor(max));
	if (!revealed || n <= rest) {
		const shown = Math.min(n, rest);
		return { shown, more: n - shown, carousel: false };
	}
	if (n <= cap) return { shown: n, more: 0, carousel: false };
	if (reducedMotion) return { shown: cap, more: n - cap, carousel: false };
	return { shown: n, more: n - cap, carousel: true };
}

/**
 * Seconds for one loop of the carousel: the chips' own width plus the gap before the copy chasing
 * them, at `speed` px/s, so a row of many chips takes longer instead of moving faster. 0 when there
 * is nothing to move.
 */
export function loopDuration(trackPx: number, gapPx: number, speed = CHIP_SPEED_PX_S): number {
	if (!(trackPx > 0) || !(speed > 0)) return 0;
	return (trackPx + Math.max(0, gapPx)) / speed;
}
