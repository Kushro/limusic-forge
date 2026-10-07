// node --experimental-strip-types ui/src/lib/facets.check.ts
import type { SongItem } from './api.ts';
import {
	addedDate,
	applyFacets,
	artistCounts,
	dayToSecs,
	facetsActive,
	fold,
	NO_FACETS,
	playlistCounts,
	primaryArtist,
	regexFilter,
	secs,
	secsToDay,
	sortEverywhere,
	type EverywhereSort
} from './facets.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}
const s = (video_id: string, title: string, artists: string, duration: string, is_video = false): SongItem =>
	({ video_id, title, artists, duration, is_video }) as SongItem;
const items = [
	s('a', 'Canción', 'Ñandú & Friend', '3:00'),
	s('b', 'Two', 'Beta', '5:30', true),
	s('a', 'Canción', 'Ñandú', '3:00'),
	s('c', 'Three', 'beta, Gamma', '1:02:00')
];
const ids = (xs: SongItem[]) => xs.map((x) => x.video_id).join('');
const ctx = { copies: new Map([['a', 2], ['b', 1], ['c', 1]]), elsewhere: (v: string) => v === 'c' };

ok(secs('3:45') === 225 && secs('1:02:03') === 3723 && secs('Cast of EPIC: The Musical') === null, 'secs');
ok(fold('Canción ÑANDÚ') === 'cancion nandu', 'fold');
ok(primaryArtist('Future & Metro Boomin') === 'Future', 'primary artist');
ok(!facetsActive(NO_FACETS) && applyFacets(items, NO_FACETS, ctx) === items, 'No facets: the list untouched');

const counts = artistCounts(items);
ok(counts.map((c) => `${c.name}:${c.count}`).join(' ') === 'Beta:2 Ñandú:2', 'Artist counts fold case; ties alphabetical');

ok(ids(applyFacets(items, { ...NO_FACETS, artists: ['beta'] }, ctx)) === 'bc', 'Artist facet');
ok(ids(applyFacets(items, { ...NO_FACETS, minMin: 4 }, ctx)) === 'bc', 'At least 4 minutes');
ok(ids(applyFacets(items, { ...NO_FACETS, minMin: 4, maxMin: 10 }, ctx)) === 'b', 'Between 4 and 10');
ok(ids(applyFacets(items, { ...NO_FACETS, dupes: 'repeated' }, ctx)) === 'aa', 'In this list twice');
ok(ids(applyFacets(items, { ...NO_FACETS, dupes: 'elsewhere' }, ctx)) === 'c', 'In another playlist');
ok(ids(applyFacets(items, { ...NO_FACETS, kind: 'videos' }, ctx)) === 'b', 'Music videos only');
ok(ids(applyFacets(items, { ...NO_FACETS, kind: 'songs', artists: ['beta'] }, ctx)) === 'c', 'Facets combine');

// The global view's facets: each song once, with its playlists and first-seen date.
const g = [
	{ ...s('p', 'P', 'X', '3:00'), unavailable: true },
	s('q', 'Q', 'X', '3:00'),
	s('r', 'R', 'X', '3:00'),
	s('u', 'U', 'X', '3:00')
] as SongItem[];
const where: Record<string, string[]> = { p: ['VL1', 'VL2'], q: ['VL2'], r: ['VL3'], u: [] };
const seen: Record<string, number | null> = { p: 100, q: 200, r: null, u: 300 };
const gctx = {
	copies: new Map<string, number>(),
	elsewhere: () => false,
	playlistsOf: (v: string) => where[v] ?? [],
	firstSeen: (v: string) => seen[v] ?? null
};
ok(ids(applyFacets(g, { ...NO_FACETS, playlists: ['VL2'] }, gctx)) === 'pq', 'In a chosen playlist');
ok(ids(applyFacets(g, { ...NO_FACETS, playlists: ['VL1', 'VL3'] }, gctx)) === 'pr', 'In any of the chosen');
ok(ids(applyFacets(g, { ...NO_FACETS, status: 'unavailable' }, gctx)) === 'p', 'Unavailable only');
ok(ids(applyFacets(g, { ...NO_FACETS, status: 'available' }, gctx)) === 'qru', 'Available only');
ok(ids(applyFacets(g, { ...NO_FACETS, spread: 'several' }, gctx)) === 'p', 'In two or more');
ok(ids(applyFacets(g, { ...NO_FACETS, spread: 'one' }, gctx)) === 'qr', 'In exactly one');
ok(ids(applyFacets(g, { ...NO_FACETS, seenFrom: 150 }, gctx)) === 'qu', 'Seen from: undated left out');
ok(ids(applyFacets(g, { ...NO_FACETS, seenTo: 200 }, gctx)) === 'pq', 'Seen up to, inclusive');
ok(ids(applyFacets(g, { ...NO_FACETS, seenFrom: 200, seenTo: 200 }, gctx)) === 'q', 'One-second range');
ok(ids(applyFacets(g, { ...NO_FACETS, spread: 'one', status: 'available', seenTo: 250 }, gctx)) === 'q', 'Global facets combine');
ok(facetsActive({ ...NO_FACETS, seenTo: 0 }) && facetsActive({ ...NO_FACETS, spread: 'one' }), 'New facets count as active');
// On a playlist page there is no global context: those facets have nothing to go on.
ok(ids(applyFacets(g, { ...NO_FACETS, playlists: ['VL9'], spread: 'several', seenFrom: 999 }, ctx)) === 'pqru', 'No context: skipped');
// Date added: the Data API's date where it has one, else first seen; songs with neither left out.
const added: Record<string, number | null> = { p: 400, q: null, r: 50, u: null };
const actx = { ...gctx, addedAt: (v: string) => added[v] ?? null };
ok(addedDate('p', actx) === 400 && addedDate('q', actx) === 200 && addedDate('r', actx) === 50, 'Added date, else first seen');
ok(addedDate('r', gctx) === null && addedDate('u', {}) === null, 'Neither date: null');
ok(ids(applyFacets(g, { ...NO_FACETS, addedFrom: 250 }, actx)) === 'pu', 'Added since: the real date wins over first seen');
ok(ids(applyFacets(g, { ...NO_FACETS, addedTo: 200 }, actx)) === 'qr', 'Added until, inclusive, falling back to first seen');
ok(ids(applyFacets(g, { ...NO_FACETS, addedFrom: 300, addedTo: 400 }, actx)) === 'pu', 'Added between');
ok(ids(applyFacets(g, { ...NO_FACETS, addedTo: 250 }, gctx)) === 'pq', 'No Data API dates: first seen alone');
ok(ids(applyFacets(g, { ...NO_FACETS, addedFrom: 1, seenTo: 150 }, actx)) === 'p', 'Added and first seen combine');
ok(facetsActive({ ...NO_FACETS, addedFrom: 0 }) && facetsActive({ ...NO_FACETS, addedTo: 0 }), 'Date added counts as active');
ok(ids(applyFacets(g, { ...NO_FACETS, addedFrom: 999 }, ctx)) === 'pqru', 'No dates in the context: skipped');
// Downloaded: by the host's callback, which a page without download state does not pass.
const dctx = { ...gctx, downloaded: (v: string) => v === 'q' || v === 'u' };
ok(ids(applyFacets(g, { ...NO_FACETS, downloaded: 'yes' }, dctx)) === 'qu', 'Downloaded only');
ok(ids(applyFacets(g, { ...NO_FACETS, downloaded: 'no' }, dctx)) === 'pr', 'Not downloaded only');
ok(ids(applyFacets(g, { ...NO_FACETS, downloaded: 'yes', status: 'available', seenTo: 250 }, dctx)) === 'q', 'Downloaded combines');
ok(ids(applyFacets(g, { ...NO_FACETS, downloaded: 'all' }, dctx)) === 'pqru', 'Downloaded: all keeps everything');
ok(facetsActive({ ...NO_FACETS, downloaded: 'no' }), 'Downloaded counts as active');
ok(ids(applyFacets(g, { ...NO_FACETS, downloaded: 'yes' }, gctx)) === 'pqru', 'No download context: skipped');
const counts2 = playlistCounts([{ playlists: ['VL1', 'VL2'] }, { playlists: ['VL2', 'VL2'] }]);
ok(counts2.get('VL1') === 1 && counts2.get('VL2') === 2, 'Playlist counts, each song once');
const day = dayToSecs('2026-10-06');
const dayEnd = dayToSecs('2026-10-06', true);
ok(day !== null && secsToDay(day) === '2026-10-06' && dayEnd !== null && secsToDay(dayEnd) === '2026-10-06', 'Date round trip');
ok(dayEnd !== null && secsToDay(dayEnd + 1) === '2026-10-07', 'A day ends at its last second');
const rows = g.map((song) => ({ song, playlists: where[song.video_id], first_seen: seen[song.video_id] }));
const order = (by: EverywhereSort) => sortEverywhere(rows, by).map((r) => r.song.video_id).join('');
ok(sortEverywhere(rows, 'playlists') === rows, 'Backend order kept as is');
ok(order('newest') === 'uqpr' && order('oldest') === 'pqur', 'By first seen, undated last');
ok(order('title') === 'pqru', 'By title');
const byArtist = sortEverywhere(
	[s('1', 'B', 'zed', '1:00'), s('2', 'A', 'Ánna', '1:00'), s('3', 'A', 'zed', '1:00')].map((song) => ({ song, first_seen: null })),
	'artist'
);
ok(byArtist.map((r) => r.song.video_id).join('') === '231', 'By artist (folded), then title');
ok(dayToSecs('') === null && dayToSecs('nope') === null && secsToDay(null) === '', 'Empty dates');

ok(ids(regexFilter(items, '^t(wo|hree)$').items) === 'bc', 'Regex over titles');
ok(regexFilter(items, '(').error, 'A broken pattern says so');
ok(regexFilter(items, '').items === items, 'Empty pattern: untouched');

console.log('ok');
