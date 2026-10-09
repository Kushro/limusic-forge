// Self-check for the alerts page's pure logic in `alerts.ts`. Node 22 runs TypeScript directly:
//
//     node --experimental-strip-types ui/src/lib/alerts.check.ts
//
// Prints "ok" and exits 0, or throws on the first broken invariant. Not imported by the app, so it
// never reaches the bundle.
import type { AlertKind, PlaylistAlert, TimelineEntry } from './api.ts';
import {
	ALERT_KINDS,
	alertPlaylists,
	countByKind,
	daysAgo,
	filterAlerts,
	groupByDay,
	isAlertKind,
	isResolved,
	resolvedKeys,
	timelineSums,
	unavailableCounts,
	unseenIds,
	utcDay,
	ytmSongUrl
} from './alerts.ts';

function ok(cond: unknown, msg: string): asserts cond {
	if (!cond) throw new Error(`alerts.check: ${msg}`);
}
const same = (a: unknown, b: unknown, msg: string) =>
	ok(JSON.stringify(a) === JSON.stringify(b), `${msg}: ${JSON.stringify(a)} != ${JSON.stringify(b)}`);

const DAY = 86_400;
// 2026-10-06T12:00:00Z
const NOON = Date.UTC(2026, 9, 6, 12) / 1000;

let next = 1;
function alert(
	kind: AlertKind,
	playlist: string,
	at: number,
	extra: Partial<PlaylistAlert> = {}
): PlaylistAlert {
	const id = next++;
	return { id, playlist_id: playlist, video_id: `v${id}`, kind, song: null, at, seen: false, ...extra };
}

// Newest first, as the backend answers.
const list: PlaylistAlert[] = [
	alert('removed', 'PL_A', NOON + 60),
	alert('added', 'PL_B', NOON, { seen: true }),
	alert('removed', 'PL_A', NOON - DAY, { dismissed: true }),
	alert('moved', 'PL_A', NOON - DAY - 60, { seen: true }),
	alert('unavailable', 'PL_C', NOON - 3 * DAY)
];

// --- filterAlerts -------------------------------------------------------------------------------
same(filterAlerts(list).map((a) => a.id), [1, 2, 3, 4, 5], 'no filter keeps all, in order');
same(filterAlerts(list, { kinds: [] }).length, 5, 'an empty kind set is no kind filter');
same(filterAlerts(list, { kinds: ['removed'] }).map((a) => a.id), [1, 3], 'by one kind');
same(
	filterAlerts(list, { kinds: ['removed', 'unavailable'] }).map((a) => a.id),
	[1, 3, 5],
	'by several kinds'
);
same(filterAlerts(list, { playlist: 'PL_A' }).map((a) => a.id), [1, 3, 4], 'by playlist');
same(filterAlerts(list, { playlist: null }).length, 5, 'a null playlist is every playlist');
same(filterAlerts(list, { unseenOnly: true }).map((a) => a.id), [1, 3, 5], 'unseen only');
same(filterAlerts(list, { dismissed: false }).map((a) => a.id), [1, 2, 4, 5], 'without dismissed');
same(
	filterAlerts(list, { kinds: ['removed'], playlist: 'PL_A', unseenOnly: true, dismissed: false }).map(
		(a) => a.id
	),
	[1],
	'filters combine'
);
same(filterAlerts([], { kinds: ['added'] }), [], 'nothing in, nothing out');

// --- countByKind --------------------------------------------------------------------------------
same(
	countByKind(list),
	{ removed: 2, unavailable: 1, added: 1, moved: 1, restored: 0 },
	'counts every kind, zero included'
);
same(Object.keys(countByKind([])).sort(), [...ALERT_KINDS].sort(), 'every kind is a key');
ok(isAlertKind('moved') && !isAlertKind('deleted') && !isAlertKind(null), 'isAlertKind');

// --- alertPlaylists / unseenIds -----------------------------------------------------------------
same(
	alertPlaylists(list),
	[
		{ id: 'PL_A', count: 3 },
		{ id: 'PL_B', count: 1 },
		{ id: 'PL_C', count: 1 }
	],
	'playlists by count, ties by id'
);
same(unseenIds(list), [1, 3, 5], 'unseen ids');
same(unseenIds([{ ...list[0], id: undefined }]), [], 'a row without an id has nothing to mark');

// --- resolvedKeys / isResolved / unavailableCounts ----------------------------------------------
{
	const on = (kind: AlertKind, video: string, at: number, extra: Partial<PlaylistAlert> = {}) =>
		alert(kind, 'PL_R', at, { video_id: video, ...extra });
	const goneThenBack = on('unavailable', 'x', 10);
	const back = on('restored', 'x', 20);
	const backFirst = on('restored', 'y', 10);
	const goneAfter = on('unavailable', 'y', 20); // its restore came before it: still open
	const alone = on('unavailable', 'z', 10);
	const tie = on('unavailable', 'w', 30); // same `at`, the restore filed after it
	const tieBack = on('restored', 'w', 30);
	const elsewhere = alert('unavailable', 'PL_S', 5, { video_id: 'x' }); // another playlist
	const rows = [tieBack, tie, goneAfter, back, goneThenBack, backFirst, alone, elsewhere];
	const keys = resolvedKeys(rows);
	same(
		rows.filter((a) => isResolved(a, keys)).map((a) => a.id),
		[tie.id, goneThenBack.id],
		'an unavailable followed by a restore is resolved, alone or restored before it is not'
	);
	ok(!isResolved(back, keys) && !isResolved(alone, keys), 'restores and lone unavailables stay open');
	same(unavailableCounts(rows, keys), { open: 3, resolved: 2 }, 'open and resolved unavailables');
	same(unavailableCounts(list), { open: 1, resolved: 0 }, 'without a restore, every one is open');
	// The backend's flag wins over what the loaded rows say (it sees past the loaded pages).
	ok(isResolved({ ...alone, resolved: true }, keys), 'the backend says resolved');
	ok(!isResolved({ ...goneThenBack, resolved: false }, keys), 'the backend says open');
	ok(!isResolved({ ...back, resolved: true }), 'only an unavailable can be resolved');
	same(unavailableCounts([{ ...alone, resolved: true }]), { open: 0, resolved: 1 }, 'flag counted');
	// Gone again after coming back: the new alert is open, the old one stays resolved.
	const again = on('unavailable', 'x', 40);
	same(
		[...resolvedKeys([again, ...rows])].sort(),
		[...keys].sort(),
		'a restore only answers the unavailables before it'
	);
}

// --- groupByDay / daysAgo -----------------------------------------------------------------------
const days = groupByDay(list, utcDay);
same(
	days.map((d) => [d.day, d.items.map((a) => a.id)]),
	[
		['2026-10-06', [1, 2]],
		['2026-10-05', [3, 4]],
		['2026-10-03', [5]]
	],
	'grouped by day, newest first'
);
same(groupByDay([], utcDay), [], 'no days for no alerts');
same(utcDay(Date.UTC(2026, 0, 1) / 1000 - 1), '2025-12-31', 'a second before midnight is the day before');
same(daysAgo('2026-10-06', '2026-10-06'), 0, 'today');
same(daysAgo('2026-10-05', '2026-10-06'), 1, 'yesterday');
same(daysAgo('2026-02-28', '2026-03-01'), 1, 'across a month');
same(daysAgo('2025-12-31', '2026-01-01'), 1, 'across a year');

// --- timelineSums / ytmSongUrl ------------------------------------------------------------------
const entry: TimelineEntry = {
	snapshot_id: 7,
	taken_at: NOON,
	item_count: 40,
	title: 'Mix',
	baseline: false,
	added: 2,
	removed: 1,
	moved: 0,
	unavailable: 0,
	restored: 1,
	changes: []
};
same(
	timelineSums(entry).map((p) => `${p.sign}${p.n}`),
	['+2', '−1', '↺1'],
	'only the kinds with something, in a fixed order'
);
same(timelineSums({ ...entry, added: 0, removed: 0, restored: 0 }), [], 'no changes, no sums');
same(ytmSongUrl('a b'), 'https://music.youtube.com/watch?v=a%20b', 'song url');

console.log('ok');
