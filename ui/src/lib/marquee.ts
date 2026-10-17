// When a line too wide for its box (a track title, an artist line) scrolls instead of truncating.
// One setting per surface, the player bar and the mini player, kept in the local appearance store.
// Plain functions, no runes, so marquee.check.ts can run them under Node; Marquee.svelte draws them.
import { loopDuration } from './chips.ts';

/** auto: three passes on each track change, looping while hovered. hover: only while hovered. off: never. */
export const MARQUEE_MODES = ['auto', 'hover', 'off'] as const;
export type MarqueeMode = (typeof MARQUEE_MODES)[number];

/** Blank space between the text and the copy chasing it, in px. */
export const MARQUEE_GAP_PX = 40;

/** A stored mode, or `auto` for anything else (missing, older, hand-edited). */
export function parseMarqueeMode(raw: unknown): MarqueeMode {
	return MARQUEE_MODES.includes(raw as MarqueeMode) ? (raw as MarqueeMode) : 'auto';
}

/**
 * still: the line sits at its start, not moving. intro: the three passes that follow a track
 * change. loop: scrolling for as long as the pointer is on it.
 */
export type MarqueePhase = 'still' | 'intro' | 'loop';

export interface MarqueeState {
	mode: MarqueeMode;
	/** The text is wider than its box. A line that fits never moves. */
	overflow: boolean;
	/** The OS asked for less motion: nothing moves on its own, a hover (asked for) still does. */
	reducedMotion: boolean;
	hovering: boolean;
	/** The passes for the current text have run out, or the pointer cut them short. */
	introDone: boolean;
}

export function marqueePhase(s: MarqueeState): MarqueePhase {
	if (!s.overflow || s.mode === 'off') return 'still';
	if (s.hovering) return 'loop';
	if (s.mode === 'auto' && !s.reducedMotion && !s.introDone) return 'intro';
	return 'still';
}

/** Seconds for one pass over a `textPx`-wide line, at the carousel's constant 30 px/s. */
export function marqueeDuration(textPx: number): number {
	return loopDuration(textPx, MARQUEE_GAP_PX);
}
