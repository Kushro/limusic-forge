// localStorage travels inside a profile file (webstorage.json). Export reads it here; the first
// launch after an import writes it back here, then reloads once so every store that read
// localStorage at module load (theme, locale, columns…) starts from the imported values.

/** Keep in sync with WEBSTORAGE_KEYS in src-tauri/src/profile/mod.rs. */
export const WEBSTORAGE_KEYS = [
	'limusic:personal',
	'primary-theme',
	'custom-theme',
	'appearance',
	'limusic_locale',
	'limusic:effect-presets',
	'zoom',
	'sidebar_collapsed',
	'track_columns',
	'playlist_sort',
	'lyrics_size',
	'lyrics_romanized',
	'lyrics_translated',
	'new_playlist_where',
	'playlist_suggestions_hidden',
	'lt_name'
] as const;

export function collectWebStorage(): Record<string, string> {
	const out: Record<string, string> = {};
	for (const key of WEBSTORAGE_KEYS) {
		try {
			const v = localStorage.getItem(key);
			if (v !== null) out[key] = v;
		} catch {
			// Unreadable key: left out of the profile.
		}
	}
	return out;
}

/** Writes the imported values over the current ones. Returns how many were written. */
export function applyWebStorage(map: Record<string, string>): number {
	let n = 0;
	for (const key of WEBSTORAGE_KEYS) {
		if (!Object.hasOwn(map, key)) continue;
		try {
			localStorage.setItem(key, map[key]);
			n++;
		} catch {
			// Quota or a blocked store: the rest still goes in.
		}
	}
	return n;
}

let taken = false;

/**
 * Called once from the root layout at startup. Rust hands the staged localStorage over exactly once
 * (it deletes it on read), so the reload below cannot loop: the next launch gets `null`.
 */
export async function applyStagedWebStorage(
	take: () => Promise<Record<string, string> | null>
): Promise<void> {
	if (taken) return;
	taken = true;
	try {
		const map = await take();
		if (map && applyWebStorage(map) > 0) location.reload();
	} catch {
		// No staged storage, or an older backend: nothing to apply.
	}
}
