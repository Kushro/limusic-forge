// Self-check for the "+N" dialog's logic (`occurrences.ts`). Same deal as `queue.check.ts` — no
// test runner in `ui/`, node 22 runs TypeScript directly:
//
//     node --experimental-strip-types ui/src/lib/occurrences.check.ts
//
// Prints "ok" and exits 0, or throws on the first broken invariant. What it guards: the count on
// the Apply button is what really gets touched (a move skips the copies already in the
// destination), and a picked copy is found again in a fresh read, with the anchor an undo needs.
import type { Occurrence, SongItem } from './api';
import { occKey, planOccurrences, resolveRows } from './occurrences.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}
const o = (playlist_id: string, position: number | null, set_video_id: string | null, nth = 0): Occurrence => ({
	playlist_id,
	position,
	set_video_id,
	nth
});

// --- planOccurrences ---------------------------------------------------------------------------
const occ = [o('A', 1, 'a1', 0), o('A', 4, 'a4', 1), o('B', 0, 'b0'), o('C', null, null)];
const all = new Set(occ.map(occKey));
const none = new Set<string>();

ok(occKey(occ[0]) !== occKey(occ[1]), 'Two copies in one playlist are two keys');
ok(planOccurrences(occ, none, 'remove', null).count === 0, 'Nothing ticked: N = 0');
{
	const p = planOccurrences(occ, all, 'remove', null);
	ok(p.count === 4 && p.skipped === 0, 'Remove: every ticked copy');
	ok([...p.byPlaylist.keys()].join() === 'A,B,C', 'Grouped by playlist, in order');
	ok(p.byPlaylist.get('A')?.length === 2, 'Both copies of A');
}
{
	const p = planOccurrences(occ, all, 'move', 'A');
	ok(p.count === 2 && p.skipped === 2, 'Move to A: its own two copies are skipped');
	ok(!p.byPlaylist.has('A'), 'Nothing taken out of the destination');
}
{
	const p = planOccurrences(occ, new Set([occKey(occ[0])]), 'move', 'A');
	ok(p.count === 0 && p.skipped === 1, 'Only copies already there: N = 0, one skipped');
}
ok(planOccurrences(occ, all, 'move', null).count === 0, 'Move with no destination: N = 0');
ok(planOccurrences(occ, all, 'move', 'Z').count === 4, 'Move elsewhere: all of them');
{
	const p = planOccurrences(occ, all, 'move_new', null);
	ok(p.count === 4 && p.skipped === 0, 'Move to new: all of them, none skipped');
}

// --- resolveRows -------------------------------------------------------------------------------
const row = (video_id: string, set_video_id?: string): SongItem => ({ video_id, title: video_id, artists: '', set_video_id });
const fresh = [row('x', 's0'), row('v', 's1'), row('y', 's2'), row('v', 's3'), row('z', 's4')];
const pairs = (rows: { song: SongItem; before: string | null }[]) =>
	rows.map((r) => `${r.song.set_video_id}>${r.before}`).join(' ');

ok(pairs(resolveRows(fresh, 'v', [o('A', 1, 's1', 0)])) === 's1>s2', 'By handle, anchored on the next row');
ok(pairs(resolveRows(fresh, 'v', [o('A', 3, 'gone', 1)])) === 's3>s4', 'Handle changed: the nth copy');
ok(
	pairs(resolveRows(fresh, 'v', [o('A', 3, 's3', 1), o('A', 1, 's1', 0)])) === 's1>s2 s3>s4',
	'Both copies, in playlist order'
);
ok(pairs(resolveRows([row('v', 's1'), row('v', 's2')], 'v', [o('A', 0, 'q', 0), o('A', 1, 'r', 1)])) === 's1>null s2>null',
	'Adjacent copies both go: the anchor skips them, none stays after');
ok(pairs(resolveRows(fresh, 'v', [o('A', 9, 'gone', 5)])) === '', 'A copy no longer there: left out');
ok(pairs(resolveRows(fresh, 'v', [o('A', 1, 's1', 0), o('A', 1, 's1', 0)])) === 's1>s2', 'One row claimed twice: once');
ok(pairs(resolveRows([row('v')], 'v', [o('A', 0, null, 0)])) === '', 'No handle: not editable, left out');
ok(pairs(resolveRows(fresh, 'v', [o('A', null, null, 0)])) === 's1>s2', 'No snapshot position: first copy');

console.log('ok');
