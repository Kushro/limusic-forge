// The dynamic mini player (MiniDynamic.svelte): what Rust told it at open, and when its controls
// are up. Plain functions, no runes, so minidyn.check.ts can run them under Node.

export type MiniMode = 'classic' | 'dynamic';
export type MiniLyrics = 'overlay' | 'side';

export interface MiniConfig {
	mode: MiniMode;
	/** The thin position line over the picture while the controls are down. */
	progress: boolean;
	/** Where the lyrics go when opened: over the lower half, or a column on the right. */
	lyrics: MiniLyrics;
}

const DEFAULTS: MiniConfig = { mode: 'classic', progress: true, lyrics: 'overlay' };

/** What Rust injected as `window.__LIMUSIC_MINI__` (src-tauri/src/mini.rs), field by field: a
 *  field that is missing or not one of its values falls back on its own, so a bad `lyrics` never
 *  costs the mode. Anything that is not an object is the classic widget with its defaults. */
export function parseMiniConfig(raw: unknown): MiniConfig {
	if (!raw || typeof raw !== 'object') return { ...DEFAULTS };
	const r = raw as Record<string, unknown>;
	return {
		mode: r.mode === 'dynamic' || r.mode === 'classic' ? r.mode : DEFAULTS.mode,
		progress: typeof r.progress === 'boolean' ? r.progress : DEFAULTS.progress,
		lyrics: r.lyrics === 'side' || r.lyrics === 'overlay' ? r.lyrics : DEFAULTS.lyrics
	};
}

/** The config of this window. The script runs before the page does, so this is settled by the
 *  first render; the main window has none and reads as classic, which it never asks about. */
export const miniConfig = (): MiniConfig =>
	parseMiniConfig((globalThis as { __LIMUSIC_MINI__?: unknown }).__LIMUSIC_MINI__);

/** How long the controls stay up after the pointer stops moving over the picture. */
export const IDLE_MS = 2000;

export interface ControlsState {
	/** performance.now() of the last pointer move over the window. */
	lastMoveAt: number;
	now: number;
	/** The pointer is over the window. */
	inside: boolean;
	/** A seek or volume drag is in progress, wherever the pointer has wandered to. */
	dragging: boolean;
	/** Keyboard focus is on one of the controls. */
	focusWithin: boolean;
}

/** Whether the control layer is showing. A drag or keyboard focus holds it up regardless; past
 *  that it takes a pointer inside the window that moved less than `idleMs` ago. */
export function controlsVisible(s: ControlsState, idleMs = IDLE_MS): boolean {
	if (s.dragging || s.focusWithin) return true;
	return s.inside && s.now - s.lastMoveAt < idleMs;
}
