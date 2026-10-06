// Narrowing a track list by more than a search box: artist, length, duplicates, songs vs music
// videos, and a regex mode for the search itself. PlaylistForge's playlist filters, as chips under
// the playlist header; Library ▸ In your playlists adds which playlists, availability, first seen
// and spread, and downloaded where the host knows. Every facet combines with the others (AND).
// Pure, so `facets.check.ts`
// runs it under plain Node.
import type { SongItem } from './api';

export type Facets = {
	/** Primary artists to keep (lowercased); empty keeps everyone. */
	artists: string[];
	/** Length bounds in minutes, inclusive; null for no bound. */
	minMin: number | null;
	maxMin: number | null;
	/** `repeated`: in this list more than once. `elsewhere`: also in another playlist of yours. */
	dupes: 'all' | 'repeated' | 'elsewhere';
	kind: 'all' | 'songs' | 'videos';
	// The rest only mean something in Library ▸ In your playlists, where each song carries the
	// playlists holding it and when it was first seen in one (`FacetContext.playlistsOf`/`firstSeen`).
	/** Playlist ids: keep songs in any of them; empty keeps everything. */
	playlists: string[];
	/** By the row's `unavailable` flag (taken down, private, blocked where you are). */
	status: 'all' | 'available' | 'unavailable';
	/** First-seen bounds in epoch seconds, inclusive; null for no bound. With either set, songs with
	 *  no known first-seen date are left out. */
	seenFrom: number | null;
	seenTo: number | null;
	/** `several`: in two or more of your playlists. `one`: in exactly one. */
	spread: 'all' | 'several' | 'one';
	/** Whether the video has a downloaded file, by `FacetContext.downloaded`; skipped without it. */
	downloaded: 'all' | 'yes' | 'no';
};

export const NO_FACETS: Facets = {
	artists: [],
	minMin: null,
	maxMin: null,
	dupes: 'all',
	kind: 'all',
	playlists: [],
	status: 'all',
	seenFrom: null,
	seenTo: null,
	spread: 'all',
	downloaded: 'all'
};

export function facetsActive(f: Facets): boolean {
	return (
		f.artists.length > 0 ||
		f.minMin !== null ||
		f.maxMin !== null ||
		f.dupes !== 'all' ||
		f.kind !== 'all' ||
		f.playlists.length > 0 ||
		f.status !== 'all' ||
		f.seenFrom !== null ||
		f.seenTo !== null ||
		f.spread !== 'all' ||
		f.downloaded !== 'all'
	);
}

/** "3:45" or "1:02:03" in seconds; null for anything else. */
export function secs(d: string | undefined | null): number | null {
	if (!d || !/^\d+(:\d+){0,2}$/.test(d.trim())) return null;
	return d
		.trim()
		.split(':')
		.reduce((acc, p) => acc * 60 + Number(p), 0);
}

/** Accents and case folded away: "Canción" finds "cancion" and the other way round. */
export function fold(s: string): string {
	return s.normalize('NFKD').replace(/\p{M}/gu, '').toLowerCase();
}

/** The first name on an artist line, as written: "Future & Metro Boomin" → "Future". */
export function primaryArtist(artists: string | undefined): string {
	return [', ', ' & ', ' • ', ' x ', ' feat. ', ' ft. ', ' and ']
		.reduce((s, sep) => s.split(sep)[0], artists ?? '')
		.trim();
}

/** Each primary artist with how many tracks it has here, most first. */
export function artistCounts(items: SongItem[]): { key: string; name: string; count: number }[] {
	const by = new Map<string, { key: string; name: string; count: number }>();
	for (const s of items) {
		const name = primaryArtist(s.artists);
		if (!name) continue;
		const key = fold(name);
		const e = by.get(key);
		if (e) e.count++;
		else by.set(key, { key, name, count: 1 });
	}
	return [...by.values()].sort((a, b) => b.count - a.count || a.name.localeCompare(b.name));
}

export type FacetContext = {
	/** How many times each video is in this list. */
	copies: Map<string, number>;
	/** Whether a video is also in another playlist of yours. */
	elsewhere: (videoId: string) => boolean;
	/** The playlists holding a video (the global view). Without it the `playlists` and `spread`
	 *  facets have nothing to go on and are skipped. */
	playlistsOf?: (videoId: string) => string[];
	/** When a video was first seen in any playlist, epoch seconds or null. Without it the first-seen
	 *  range is skipped. */
	firstSeen?: (videoId: string) => number | null;
	/** Whether a video has a downloaded file. Without it the `downloaded` facet is skipped. */
	downloaded?: (videoId: string) => boolean;
};

/** How many songs each playlist holds among `rows`, keyed by playlist id. */
export function playlistCounts(rows: { playlists: string[] }[]): Map<string, number> {
	const out = new Map<string, number>();
	for (const r of rows) for (const p of new Set(r.playlists)) out.set(p, (out.get(p) ?? 0) + 1);
	return out;
}

/** A `<input type="date">` value ("2026-10-06") as epoch seconds at the start of that local day,
 *  or at its last second with `end`. Null for an empty or malformed value. */
export function dayToSecs(day: string, end = false): number | null {
	const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day.trim());
	if (!m) return null;
	// The next midnight less a second, not +86399: a day with a clock change is 23 or 25 hours.
	const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]) + (end ? 1 : 0));
	if (isNaN(d.getTime())) return null;
	return Math.floor(d.getTime() / 1000) - (end ? 1 : 0);
}

/** Epoch seconds as the local day an `<input type="date">` shows; '' for null. */
export function secsToDay(secs: number | null): string {
	if (secs === null) return '';
	const d = new Date(secs * 1000);
	const pad = (n: number) => String(n).padStart(2, '0');
	return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** Orders for Library ▸ In your playlists. `playlists` is the backend's: most playlists first, then
 *  by title. By first seen, undated songs go last either way. */
export type EverywhereSort = 'playlists' | 'newest' | 'oldest' | 'title' | 'artist';

export function sortEverywhere<T extends { song: SongItem; first_seen: number | null }>(
	rows: T[],
	by: EverywhereSort
): T[] {
	if (by === 'playlists') return rows;
	const title = (r: T) => fold(r.song.title ?? '');
	const cmp: (a: T, b: T) => number =
		by === 'title'
			? (a, b) => title(a).localeCompare(title(b))
			: by === 'artist'
				? (a, b) =>
						fold(primaryArtist(a.song.artists)).localeCompare(fold(primaryArtist(b.song.artists))) ||
						title(a).localeCompare(title(b))
				: (a, b) => {
						if (a.first_seen === b.first_seen) return 0;
						if (a.first_seen === null) return 1;
						if (b.first_seen === null) return -1;
						return by === 'newest' ? b.first_seen - a.first_seen : a.first_seen - b.first_seen;
					};
	return [...rows].sort(cmp);
}

export function applyFacets<T extends SongItem>(items: T[], f: Facets, ctx: FacetContext): T[] {
	if (!facetsActive(f)) return items;
	const artists = new Set(f.artists);
	const lo = f.minMin === null ? null : f.minMin * 60;
	const hi = f.maxMin === null ? null : f.maxMin * 60;
	const lists = new Set(f.playlists);
	const { playlistsOf, firstSeen, downloaded } = ctx;
	const ranged = f.seenFrom !== null || f.seenTo !== null;
	return items.filter((s) => {
		if (downloaded && f.downloaded !== 'all' && downloaded(s.video_id) !== (f.downloaded === 'yes'))
			return false;
		if (f.status === 'available' && s.unavailable) return false;
		if (f.status === 'unavailable' && !s.unavailable) return false;
		if (playlistsOf && (lists.size || f.spread !== 'all')) {
			const of = playlistsOf(s.video_id);
			if (lists.size && !of.some((p) => lists.has(p))) return false;
			if (f.spread === 'several' && of.length < 2) return false;
			if (f.spread === 'one' && of.length !== 1) return false;
		}
		if (ranged && firstSeen) {
			const at = firstSeen(s.video_id);
			if (at === null || (f.seenFrom !== null && at < f.seenFrom) || (f.seenTo !== null && at > f.seenTo))
				return false;
		}
		if (artists.size && !artists.has(fold(primaryArtist(s.artists)))) return false;
		if (lo !== null || hi !== null) {
			const d = secs(s.duration);
			if (d === null || (lo !== null && d < lo) || (hi !== null && d > hi)) return false;
		}
		if (f.dupes === 'repeated' && (ctx.copies.get(s.video_id) ?? 0) < 2) return false;
		if (f.dupes === 'elsewhere' && !ctx.elsewhere(s.video_id)) return false;
		if (f.kind === 'songs' && s.is_video) return false;
		if (f.kind === 'videos' && !s.is_video) return false;
		return true;
	});
}

/** The search box as a regular expression over title, artist and album. `error` when it doesn't
 *  compile, so the box can say so instead of showing nothing. */
export function regexFilter<T extends SongItem>(items: T[], pattern: string): { items: T[]; error: boolean } {
	if (!pattern.trim()) return { items, error: false };
	let re: RegExp;
	try {
		re = new RegExp(pattern, 'iu');
	} catch {
		return { items, error: true };
	}
	return {
		items: items.filter((s) => re.test(s.title ?? '') || re.test(s.artists ?? '') || re.test(s.album ?? '')),
		error: false
	};
}
