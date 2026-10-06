// node --experimental-strip-types ui/src/lib/facets.check.ts
import type { SongItem } from './api.ts';
import { applyFacets, artistCounts, facetsActive, fold, NO_FACETS, primaryArtist, regexFilter, secs } from './facets.ts';

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

ok(ids(regexFilter(items, '^t(wo|hree)$').items) === 'bc', 'Regex over titles');
ok(regexFilter(items, '(').error, 'A broken pattern says so');
ok(regexFilter(items, '').items === items, 'Empty pattern: untouched');

console.log('ok');
