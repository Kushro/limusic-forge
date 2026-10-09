// The Recover tracks assistant's pure side (Tools ▸ Recover tracks): which candidates the filters
// show, and how a selection grows by range or by filter. No runes and `.ts` imports, so the
// self-check (recover.check.ts) runs it under plain Node; the session store is recover.svelte.ts.
import type { PlaylistOp, RecoverApplied, RecoverCandidate, RecoverRow, RecoverSource } from './api.ts';
import { parseYtLink } from './ytlink.ts';

export const RECOVER_SOURCES: readonly RecoverSource[] = [
	'unavailable',
	'alert_unavailable',
	'alert_removed'
];

/** What the pick step shows. Empty `sources` and `playlist` mean all of them. */
export type RecoverFilter = {
	sources: RecoverSource[];
	playlist: string;
	text: string;
	/** List the candidates whose every alert was dismissed (D5: hidden by default). */
	includeDismissed: boolean;
};

export const emptyFilter = (): RecoverFilter => ({
	sources: [],
	playlist: '',
	text: '',
	includeDismissed: false
});

/** Lowercase without accents, so "cancion" finds "Canción". */
const fold = (s: string) => s.normalize('NFKD').replace(/\p{M}/gu, '').toLowerCase();

/** Whether a filter narrows anything beyond the dismissed switch. */
export const isFiltering = (f: RecoverFilter) =>
	f.sources.length > 0 || f.playlist !== '' || f.text.trim() !== '';

export function matchesFilter(c: RecoverCandidate, f: RecoverFilter): boolean {
	if (c.dismissed && !f.includeDismissed) return false;
	if (f.sources.length && !c.sources.some((s) => f.sources.includes(s))) return false;
	if (f.playlist && c.playlist_id !== f.playlist) return false;
	const q = fold(f.text.trim());
	if (!q) return true;
	return [c.title, c.artists, c.video_id].some((v) => !!v && fold(v).includes(q));
}

export const filterCandidates = (list: readonly RecoverCandidate[], f: RecoverFilter) =>
	list.filter((c) => matchesFilter(c, f));

/** The keys from `anchor` to `target` in list order, both ends included, whichever comes first.
 *  An anchor not in `keys` (filtered out, or none yet) makes it `target` alone; a `target` not in
 *  `keys`, nothing. */
export function rangeSelect(keys: readonly string[], anchor: string | null, target: string): string[] {
	const to = keys.indexOf(target);
	if (to < 0) return [];
	const from = anchor === null ? -1 : keys.indexOf(anchor);
	if (from < 0) return [target];
	const [lo, hi] = from <= to ? [from, to] : [to, from];
	return keys.slice(lo, hi + 1);
}

/** A click on a row: toggle it, or with Shift add the range from the anchor (as a file manager
 *  does: a range only ever selects). Answers the new selection and anchor. */
export function clickSelect(
	selected: ReadonlySet<string>,
	keys: readonly string[],
	anchor: string | null,
	target: string,
	shift: boolean
): { selected: Set<string>; anchor: string } {
	const next = new Set(selected);
	if (shift && anchor !== null && keys.includes(anchor)) {
		for (const k of rangeSelect(keys, anchor, target)) next.add(k);
	} else if (next.has(target)) {
		next.delete(target);
	} else {
		next.add(target);
	}
	return { selected: next, anchor: target };
}

/** "Select filtered": the selection plus every candidate the filters show. */
export function selectFiltered(
	selected: ReadonlySet<string>,
	shown: readonly RecoverCandidate[]
): Set<string> {
	const next = new Set(selected);
	for (const c of shown) next.add(c.key);
	return next;
}

/** The selection without keys no longer among the candidates (recovered, resolved, dismissed). */
export function retainKeys(
	selected: ReadonlySet<string>,
	list: readonly RecoverCandidate[]
): Set<string> {
	const have = new Set(list.map((c) => c.key));
	return new Set([...selected].filter((k) => have.has(k)));
}

/** The selected keys in list order: what the later steps work on. */
export const orderedSelection = (selected: ReadonlySet<string>, list: readonly RecoverCandidate[]) =>
	list.filter((c) => selected.has(c.key)).map((c) => c.key);

/** The playlists the candidates are in, each once, in the order first met. */
export function candidatePlaylists(list: readonly RecoverCandidate[]): string[] {
	return [...new Set(list.map((c) => c.playlist_id))];
}

/** How many candidates each source names (a candidate with two sources counts in both). */
export function sourceCounts(list: readonly RecoverCandidate[]): Record<RecoverSource, number> {
	const out: Record<RecoverSource, number> = { unavailable: 0, alert_unavailable: 0, alert_removed: 0 };
	for (const c of list) for (const s of c.sources) out[s]++;
	return out;
}

/** A 0-based position as a row shows it: `#1`, or `?` when unknown. */
export const positionLabel = (position: number | null) =>
	position === null ? '?' : `#${position + 1}`;

// --- the later steps (titles, search, review, apply) ------------------------------------------

/** A key's playlist and video, as `recoverKey` joined them. */
export function splitKey(key: string): { playlist_id: string; video_id: string } {
	const i = key.indexOf('\u001f');
	return i < 0 ? { playlist_id: '', video_id: key } : { playlist_id: key.slice(0, i), video_id: key.slice(i + 1) };
}

/** How the review groups the rows. `pending` holds the ones not searched yet (no title). */
export type ReviewTier = 'matched' | 'check' | 'missing' | 'pending';
export const REVIEW_TIERS: readonly ReviewTier[] = ['matched', 'check', 'missing', 'pending'];
export type ReviewFilter = ReviewTier | 'all';

export function tierCounts(rows: readonly RecoverRow[]): Record<ReviewTier, number> {
	const out: Record<ReviewTier, number> = { matched: 0, check: 0, missing: 0, pending: 0 };
	for (const r of rows) out[r.tier]++;
	return out;
}

export const filterRows = (rows: readonly RecoverRow[], f: ReviewFilter) =>
	f === 'all' ? [...rows] : rows.filter((r) => r.tier === f);

/** The rows that go in on apply: approved, with a replacement. */
export const approvedKeys = (rows: readonly RecoverRow[]) =>
	rows.filter((r) => r.approved && !!r.pick).map((r) => r.key);

/** The rows a bulk approve touches: a replacement, not approved yet, in `tier` (or any). */
export const approvableRows = (rows: readonly RecoverRow[], tier?: ReviewTier) =>
	rows.filter((r) => !!r.pick && !r.approved && (!tier || r.tier === tier));

/** Rows the title step can work on: no title yet. */
export const untitledKeys = (rows: readonly RecoverRow[]) =>
	rows.filter((r) => r.title === null).map((r) => r.key);

/** Rows the search step can work on: a title, not searched yet. */
export const searchableKeys = (rows: readonly RecoverRow[]) =>
	rows.filter((r) => r.title !== null && r.tier === 'pending').map((r) => r.key);

/** The playlists these keys are in, each once, in the order first met. */
export const keyPlaylists = (keys: readonly string[]) => [...new Set(keys.map((k) => splitKey(k).playlist_id))];

/** A pasted link or a bare 11-character ID as a videoId, or null. */
export function pastedVideoId(input: string): string | null {
	const text = input.trim();
	if (/^[A-Za-z0-9_-]{11}$/.test(text)) return text;
	const target = parseYtLink(text);
	return target?.kind === 'song' && /^[A-Za-z0-9_-]{11}$/.test(target.id) ? target.id : null;
}

/** What waiting until unix second `until` looks like at `nowMs`: `m:ss`, never below 0:00. */
export function countdown(until: number, nowMs: number): string {
	const s = Math.max(0, Math.ceil(until - nowMs / 1000));
	return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

/** What one apply did, over every playlist: its journal entries in order (for one Undo), the rows
 *  done and each failed row. */
export function appliedTotals(applied: RecoverApplied): {
	ops: PlaylistOp[];
	done: number;
	playlists: number;
	failed: { key: string; error: string }[];
} {
	const ops = applied.playlists.flatMap((p) => p.ops);
	const done = applied.playlists.reduce((n, p) => n + p.done, 0);
	const playlists = applied.playlists.filter((p) => p.done > 0).length;
	const failed = applied.playlists.flatMap((p) => p.failed.map(([key, error]) => ({ key, error })));
	return { ops, done, playlists, failed };
}
