// The Library's playlists tab: its sort (`library_playlists_sort`), its view
// (`library_playlists_view`), and the "2 h ago · +3 −1 ~2" line each playlist carries once the
// monitor has read it. Pure, so `plsort.check.ts` runs it under plain Node.
import type { PlaylistSyncInfo } from './api';
import { fold } from './facets.ts';

/** `default` is the order YouTube Music hands the library over in. */
export type PlSortKey = 'default' | 'title' | 'count' | 'synced';
export type PlSort = { key: PlSortKey; desc: boolean };
export type PlView = 'grid' | 'list';

export const PL_SORT_KEYS: PlSortKey[] = ['default', 'title', 'count', 'synced'];
export const DEFAULT_SORT: PlSort = { key: 'default', desc: false };
export const DEFAULT_VIEW: PlView = 'grid';

/** `title`, `count:desc`… Anything unknown is the default order; `default` has no direction. */
export function parseSort(s: string | null | undefined): PlSort {
	const [key, dir, ...rest] = (s ?? '').trim().split(':');
	if (rest.length || !PL_SORT_KEYS.includes(key as PlSortKey) || key === 'default')
		return { ...DEFAULT_SORT };
	if (dir !== undefined && dir !== 'desc' && dir !== 'asc') return { ...DEFAULT_SORT };
	return { key: key as PlSortKey, desc: dir === 'desc' };
}

/** The setting's value for a sort: the inverse of `parseSort`. */
export function formatSort(sort: PlSort): string {
	return sort.key === 'default' || !sort.desc ? sort.key : `${sort.key}:desc`;
}

export function parseView(s: string | null | undefined): PlView {
	return s?.trim() === 'list' ? 'list' : DEFAULT_VIEW;
}

/** What the sort needs of a playlist: a library card, or anything shaped like one. */
export type Sortable = { id: string; title: string; subtitle?: string; count?: number };

/** A playlist's sync record. Ids normally match as they are; the `VL` browse prefix is tolerated
 *  both ways, since a library card and the monitor may not spell the same playlist alike. */
export function infoFor(
	info: Record<string, PlaylistSyncInfo>,
	id: string
): PlaylistSyncInfo | undefined {
	return info[id] ?? (id.startsWith('VL') ? info[id.slice(2)] : info[`VL${id}`]);
}

/** The track count a card's own subtitle states ("You • 1,234 tracks" → 1234), if it states one. */
export function subtitleCount(subtitle: string | undefined): number | undefined {
	const last = (subtitle ?? '').split('•').pop() ?? '';
	const m = /(\d[\d,.   ]*)/.exec(last);
	if (!m) return undefined;
	const n = Number(m[1].replace(/[^\d]/g, ''));
	return Number.isFinite(n) ? n : undefined;
}

/** The count the `count` sort uses: the last sync's, else the item's own, else the subtitle's. */
export function countOf(item: Sortable, info: Record<string, PlaylistSyncInfo>): number | undefined {
	return infoFor(info, item.id)?.item_count ?? item.count ?? subtitleCount(item.subtitle);
}

/**
 * The playlists in `sort`'s order, as a new array (the input is left alone). Titles compare with
 * accents and case folded away; counts and sync times compare as numbers. A playlist with nothing
 * to compare (never synced, no count) goes last whichever the direction, and ties keep the
 * library's own order.
 */
export function sortPlaylists<T extends Sortable>(
	items: T[],
	sort: PlSort,
	info: Record<string, PlaylistSyncInfo>
): T[] {
	if (sort.key === 'default') return items.slice();
	const dir = sort.desc ? -1 : 1;
	const keyed = items.map((item, i) => {
		let k: string | number | undefined;
		if (sort.key === 'title') k = fold(item.title);
		else if (sort.key === 'count') k = countOf(item, info);
		else k = infoFor(info, item.id)?.synced_at;
		return { item, i, k };
	});
	keyed.sort((a, b) => {
		if (a.k === undefined || b.k === undefined) {
			if (a.k === undefined && b.k === undefined) return a.i - b.i;
			return a.k === undefined ? 1 : -1;
		}
		const c =
			typeof a.k === 'string'
				? a.k.localeCompare(b.k as string, undefined, { numeric: true })
				: a.k - (b.k as number);
		return c * dir || a.i - b.i;
	});
	return keyed.map((x) => x.item);
}

/** How long ago, as a unit the locale catalog words (`library.sync_ago_<unit>`) and its number. */
export type Ago = { unit: 'now' | 'minutes' | 'hours' | 'days' | 'months' | 'years'; n: number };

/** `secs` and `now` are epoch seconds. A time in the future (a clock moved back) reads as now. */
export function relativeAgo(secs: number, now: number): Ago {
	const d = Math.max(0, Math.floor(now - secs));
	if (d < 60) return { unit: 'now', n: 0 };
	if (d < 3600) return { unit: 'minutes', n: Math.floor(d / 60) };
	if (d < 86_400) return { unit: 'hours', n: Math.floor(d / 3600) };
	if (d < 30 * 86_400) return { unit: 'days', n: Math.floor(d / 86_400) };
	if (d < 365 * 86_400) return { unit: 'months', n: Math.floor(d / (30 * 86_400)) };
	return { unit: 'years', n: Math.floor(d / (365 * 86_400)) };
}

/** "+3 −1 ~2", leaving out whatever is zero; empty when the sync found nothing changed. */
export function syncLine(info: Pick<PlaylistSyncInfo, 'added' | 'removed' | 'moved'>): string {
	return [
		info.added ? `+${info.added}` : '',
		info.removed ? `−${info.removed}` : '',
		info.moved ? `~${info.moved}` : ''
	]
		.filter(Boolean)
		.join(' ');
}
