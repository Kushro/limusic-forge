// Narrowing a track list by more than a search box: artist, length, duplicates, songs vs music
// videos, and a regex mode for the search itself. PlaylistForge's playlist filters, as chips under
// the playlist header. Every facet combines with the others (AND). Pure, so `facets.check.ts`
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
};

export const NO_FACETS: Facets = { artists: [], minMin: null, maxMin: null, dupes: 'all', kind: 'all' };

export function facetsActive(f: Facets): boolean {
	return (
		f.artists.length > 0 ||
		f.minMin !== null ||
		f.maxMin !== null ||
		f.dupes !== 'all' ||
		f.kind !== 'all'
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
};

export function applyFacets<T extends SongItem>(items: T[], f: Facets, ctx: FacetContext): T[] {
	if (!facetsActive(f)) return items;
	const artists = new Set(f.artists);
	const lo = f.minMin === null ? null : f.minMin * 60;
	const hi = f.maxMin === null ? null : f.maxMin * 60;
	return items.filter((s) => {
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
