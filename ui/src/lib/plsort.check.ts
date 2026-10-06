// node --experimental-strip-types ui/src/lib/plsort.check.ts
import type { PlaylistSyncInfo } from './api.ts';
import {
	countOf,
	formatSort,
	infoFor,
	parseSort,
	parseView,
	relativeAgo,
	sortPlaylists,
	subtitleCount,
	syncLine
} from './plsort.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}
const sync = (synced_at: number, item_count: number, added = 0, removed = 0, moved = 0) =>
	({ synced_at, item_count, added, removed, moved }) as PlaylistSyncInfo;

// --- parseSort / formatSort / parseView ------------------------------------------------------
const ps = (s: string | null | undefined) => {
	const p = parseSort(s);
	return `${p.key}${p.desc ? ':desc' : ''}`;
};
ok(ps('title') === 'title' && ps('title:asc') === 'title', 'Ascending by default');
ok(ps('count:desc') === 'count:desc' && ps(' synced:desc ') === 'synced:desc', 'desc suffix');
ok(ps(null) === 'default' && ps('') === 'default' && ps(undefined) === 'default', 'Unset: default');
ok(ps('bogus') === 'default' && ps('title:sideways') === 'default', 'Unknown: default');
ok(ps('title:desc:x') === 'default', 'Extra parts: default');
ok(ps('default:desc') === 'default', 'The default order has no direction');
for (const s of ['default', 'title', 'title:desc', 'count', 'count:desc', 'synced', 'synced:desc'])
	ok(formatSort(parseSort(s)) === s, `round trip ${s}`);
ok(parseView('list') === 'list' && parseView('grid') === 'grid', 'Views');
ok(parseView(null) === 'grid' && parseView('tiles') === 'grid', 'Unknown view: grid');

// --- sortPlaylists ---------------------------------------------------------------------------
const pl = (id: string, title: string, subtitle?: string, count?: number) => ({
	id,
	title,
	subtitle,
	count
});
const items = [
	pl('a', 'Zebra'),
	pl('b', 'Ábaco', 'You • 12 tracks'),
	pl('c', 'beta'),
	pl('d', 'alpha', undefined, 7),
	pl('VLe', 'Echo')
];
const info: Record<string, PlaylistSyncInfo> = {
	a: sync(300, 40),
	c: sync(100, 3),
	e: sync(200, 3)
};
const ids = (xs: { id: string }[]) => xs.map((x) => x.id).join(',');

const before = ids(items);
ok(ids(sortPlaylists(items, parseSort('default'), info)) === before, 'Default: library order');
ok(ids(sortPlaylists(items, parseSort('title'), info)) === 'b,d,c,VLe,a', 'Title folds accents and case');
ok(ids(sortPlaylists(items, parseSort('title:desc'), info)) === 'a,VLe,c,d,b', 'Title reversed');
ok(ids(items) === before, 'The input is left alone');

ok(countOf(items[0], info) === 40, 'Count from the sync');
ok(countOf(items[1], info) === 12, 'Count from the subtitle');
ok(countOf(items[3], info) === 7, 'Count from the item');
ok(countOf(pl('x', 'x'), info) === undefined, 'No count');
ok(ids(sortPlaylists(items, parseSort('count'), info)) === 'c,VLe,d,b,a', 'Count ascending, ties stable');
ok(ids(sortPlaylists(items, parseSort('count:desc'), info)) === 'a,b,d,c,VLe', 'Count descending');
const uncounted = [pl('n', 'n'), ...items];
ok(ids(sortPlaylists(uncounted, parseSort('count'), info)).endsWith(',n'), 'Uncounted last');
ok(ids(sortPlaylists(uncounted, parseSort('count:desc'), info)).endsWith(',n'), 'Uncounted last, desc too');

ok(ids(sortPlaylists(items, parseSort('synced'), info)) === 'c,VLe,a,b,d', 'Oldest sync first; never synced last');
ok(ids(sortPlaylists(items, parseSort('synced:desc'), info)) === 'a,VLe,c,b,d', 'Newest sync first; never synced last');

ok(infoFor(info, 'VLe')?.synced_at === 200, 'VL prefix on the card');
ok(infoFor({ VLz: sync(1, 1) }, 'z')?.synced_at === 1, 'VL prefix on the record');
ok(infoFor(info, 'nope') === undefined, 'Unknown id');
ok(subtitleCount('You • 1,234 tracks') === 1234 && subtitleCount('You') === undefined, 'Subtitle count');
ok(subtitleCount(undefined) === undefined, 'No subtitle');

// --- relativeAgo -----------------------------------------------------------------------------
const ago = (d: number) => {
	const a = relativeAgo(1_000_000 - d, 1_000_000);
	return `${a.unit}:${a.n}`;
};
ok(ago(0) === 'now:0' && ago(59) === 'now:0', 'Under a minute: now');
ok(ago(-500) === 'now:0', 'The future reads as now');
ok(ago(60) === 'minutes:1' && ago(3599) === 'minutes:59', 'Minutes');
ok(ago(2 * 3600 + 10) === 'hours:2' && ago(86_399) === 'hours:23', 'Hours');
ok(ago(86_400) === 'days:1' && ago(29 * 86_400) === 'days:29', 'Days');
ok(ago(30 * 86_400) === 'months:1' && ago(364 * 86_400) === 'months:12', 'Months');
ok(ago(365 * 86_400) === 'years:1' && ago(800 * 86_400) === 'years:2', 'Years');

// --- syncLine --------------------------------------------------------------------------------
ok(syncLine(sync(0, 0, 3, 1, 2)) === '+3 −1 ~2', 'All three');
ok(syncLine(sync(0, 0, 0, 4, 0)) === '−4', 'Zeros left out');
ok(syncLine(sync(0, 0, 5, 0, 1)) === '+5 ~1', 'Zeros left out between');
ok(syncLine(sync(0, 0)) === '', 'Nothing changed: empty');

console.log('ok');
