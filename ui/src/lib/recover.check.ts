// node --experimental-strip-types ui/src/lib/recover.check.ts
import type {
	PlaylistOp,
	RecoverApplied,
	RecoverCandidate,
	RecoverRow,
	RecoverSource,
	SongItem
} from './api.ts';
import {
	appliedTotals,
	approvableRows,
	approvedKeys,
	candidatePlaylists,
	clickSelect,
	countdown,
	emptyFilter,
	filterCandidates,
	filterRows,
	isFiltering,
	keyPlaylists,
	orderedSelection,
	pastedVideoId,
	positionLabel,
	rangeSelect,
	retainKeys,
	searchableKeys,
	selectFiltered,
	sourceCounts,
	splitKey,
	tierCounts,
	untitledKeys
} from './recover.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}
const cand = (
	pid: string,
	vid: string,
	sources: RecoverSource[],
	extra: Partial<RecoverCandidate> = {}
): RecoverCandidate => ({
	key: `${pid}\u001f${vid}`,
	playlist_id: pid,
	video_id: vid,
	sources,
	alert_ids: [],
	in_playlist: true,
	position: null,
	set_video_id: null,
	next_handle: null,
	title: null,
	artists: null,
	duration: null,
	thumbnail: null,
	dismissed: false,
	...extra
});
const list = [
	cand('VLA', 'aaaaaaaaaaa', ['unavailable'], { title: 'Canción de prueba', artists: 'Beto', position: 0 }),
	cand('VLA', 'bbbbbbbbbbb', ['unavailable', 'alert_unavailable'], { title: 'Other', artists: 'Ana' }),
	cand('VLB', 'ccccccccccc', ['alert_removed'], { in_playlist: false, position: 4 }),
	cand('VLB', 'ddddddddddd', ['alert_unavailable'], { title: 'Gone', dismissed: true }),
	cand('VLC', 'eeeeeeeeeee', ['alert_removed'], { title: 'Last one', artists: 'Beto' })
];
const keys = (l: RecoverCandidate[]) => l.map((c) => c.video_id[0]).join('');

// Filters. Dismissed ones stay out unless asked for (D5).
const f = emptyFilter();
ok(!isFiltering(f), 'An empty filter narrows nothing');
ok(keys(filterCandidates(list, f)) === 'abce', 'Dismissed candidates are hidden by default');
ok(keys(filterCandidates(list, { ...f, includeDismissed: true })) === 'abcde', '"Include dismissed" lists them');
ok(keys(filterCandidates(list, { ...f, sources: ['alert_unavailable'] })) === 'b', 'Source filter, dismissed hidden');
ok(keys(filterCandidates(list, { ...f, sources: ['alert_unavailable'], includeDismissed: true })) === 'bd',
	'Source filter with dismissed');
ok(keys(filterCandidates(list, { ...f, sources: ['unavailable', 'alert_removed'] })) === 'abce',
	'Several sources: any of them matches');
ok(keys(filterCandidates(list, { ...f, playlist: 'VLB' })) === 'c', 'Playlist filter');
ok(isFiltering({ ...f, playlist: 'VLB' }) && isFiltering({ ...f, text: 'x' }), 'Narrowing filters are reported');
ok(keys(filterCandidates(list, { ...f, text: 'cancion' })) === 'a', 'Text matches titles without accents');
ok(keys(filterCandidates(list, { ...f, text: '  BETO ' })) === 'ae', 'Text matches artists, case and spaces aside');
ok(keys(filterCandidates(list, { ...f, text: 'cccc' })) === 'c', 'Text matches the videoId of an untitled one');
ok(keys(filterCandidates(list, { ...f, text: 'beto', playlist: 'VLC', sources: ['alert_removed'] })) === 'e',
	'Filters combine');

// rangeSelect, both directions.
const ks = list.map((c) => c.key);
ok(rangeSelect(ks, ks[1], ks[3]).join() === ks.slice(1, 4).join(), 'Range downwards includes both ends');
ok(rangeSelect(ks, ks[3], ks[1]).join() === ks.slice(1, 4).join(), 'Range upwards includes both ends, list order');
ok(rangeSelect(ks, ks[2], ks[2]).join() === ks[2], 'A range onto the anchor is that row');
ok(rangeSelect(ks, null, ks[2]).join() === ks[2], 'No anchor: the target alone');
ok(rangeSelect(ks, 'gone', ks[2]).join() === ks[2], 'An anchor filtered out: the target alone');
ok(rangeSelect(ks, ks[0], 'gone').length === 0, 'A target not listed selects nothing');

// Clicks: toggle, then Shift adds the range from the last click without dropping anything.
let s = clickSelect(new Set(), ks, null, ks[4], false);
ok(s.selected.size === 1 && s.anchor === ks[4], 'A click selects and anchors');
s = clickSelect(s.selected, ks, s.anchor, ks[1], true);
ok(s.selected.size === 4 && !s.selected.has(ks[0]) && s.anchor === ks[1], 'Shift+click upwards adds the range');
s = clickSelect(s.selected, ks, s.anchor, ks[2], false);
ok(s.selected.size === 3 && !s.selected.has(ks[2]), 'A plain click toggles one row off');
s = clickSelect(s.selected, ks, s.anchor, ks[3], true);
ok(s.selected.size === 4 && s.selected.has(ks[2]), 'Shift+click downwards only ever adds');
const visible = keys(filterCandidates(list, { ...f, playlist: 'VLA' }));
ok(visible === 'ab', 'Visible rows for the next case');
s = clickSelect(new Set([ks[4]]), [ks[0], ks[1]], ks[4], ks[1], true);
ok(s.selected.size === 2 && s.selected.has(ks[1]) && !s.selected.has(ks[0]),
	'Shift with the anchor filtered out toggles the row alone, never the hidden ones between');

// selectFiltered adds what the filters show and keeps the rest.
const shown = filterCandidates(list, { ...f, playlist: 'VLA' });
const sel = selectFiltered(new Set([ks[4]]), shown);
ok(sel.size === 3 && sel.has(ks[0]) && sel.has(ks[1]) && sel.has(ks[4]), 'Select filtered is a union');
ok(selectFiltered(sel, []).size === 3, 'Nothing shown adds nothing');

// Selection upkeep and display helpers.
ok(retainKeys(new Set([ks[0], 'VLZ\u001fzzzzzzzzzzz']), list).size === 1, 'Keys of vanished candidates drop');
ok(orderedSelection(new Set([ks[4], ks[0]]), list).join() === [ks[0], ks[4]].join(), 'Selection in list order');
ok(candidatePlaylists(list).join() === 'VLA,VLB,VLC', 'Each playlist once, in order');
const counts = sourceCounts(list);
ok(counts.unavailable === 2 && counts.alert_unavailable === 2 && counts.alert_removed === 2, 'Source counts');
ok(positionLabel(0) === '#1' && positionLabel(null) === '?', 'Position labels are 1-based or "?"');

// --- the later steps ---
const song = (vid: string): SongItem => ({ video_id: vid, title: vid, artists: 'X' }) as SongItem;
const row = (key: string, extra: Partial<RecoverRow> = {}): RecoverRow => ({
	key,
	title: 'T',
	artists: null,
	title_source: 'snapshot',
	tier: 'pending',
	pick: null,
	candidates: [],
	approved: false,
	status: 'idle',
	error: null,
	...extra
});
const rows = [
	row(ks[0], { tier: 'matched', pick: song('p0'), approved: true }),
	row(ks[1], { tier: 'matched', pick: song('p1') }),
	row(ks[2], { tier: 'check', pick: song('p2') }),
	row(ks[3], { tier: 'missing' }),
	row(ks[4], { title: null }),
	// Approved without a pick never goes in (Rust forces it false, but never trust the wire).
	row('VLD\u001fffffffffff', { tier: 'missing', approved: true })
];
const tc = tierCounts(rows);
ok(tc.matched === 2 && tc.check === 1 && tc.missing === 2 && tc.pending === 1, 'Tier counts');
ok(filterRows(rows, 'all').length === 6 && filterRows(rows, 'check').length === 1, 'Tier filter');
ok(approvedKeys(rows).join() === ks[0], 'Only approved rows with a pick go in');
ok(approvableRows(rows, 'matched').map((r) => r.key).join() === ks[1], '"Approve all matches": matched, not yet approved');
ok(approvableRows(rows).map((r) => r.key).join() === [ks[1], ks[2]].join(), '"Approve all": every row with a pick');
ok(untitledKeys(rows).join() === ks[4], 'Untitled rows for the title step');
ok(searchableKeys(rows).join() === '', 'Only pending rows with a title are searched');
ok(searchableKeys([row(ks[0])]).join() === ks[0], 'A titled pending row is searched');

const sk = splitKey(ks[2]);
ok(sk.playlist_id === 'VLB' && sk.video_id === 'ccccccccccc', 'A key splits into playlist and video');
ok(splitKey('abc').video_id === 'abc' && splitKey('abc').playlist_id === '', 'A bare video stays a video');
ok(keyPlaylists([ks[0], ks[1], ks[2], ks[4]]).join() === 'VLA,VLB,VLC', 'Each playlist of the keys once');

ok(pastedVideoId(' dQw4w9WgXcQ ') === 'dQw4w9WgXcQ', 'A bare ID');
ok(pastedVideoId('https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=PLx') === 'dQw4w9WgXcQ', 'A watch link');
ok(pastedVideoId('youtu.be/dQw4w9WgXcQ') === 'dQw4w9WgXcQ', 'A short link without scheme');
ok(pastedVideoId('https://youtube.com/playlist?list=PLx') === null, 'A playlist link is not a song');
ok(pastedVideoId('https://youtu.be/short') === null, 'A malformed ID is refused here');
ok(pastedVideoId('hello world') === null, 'Free text is no ID');

ok(countdown(100, 40_000) === '1:00', 'Countdown in m:ss');
ok(countdown(100, 99_500) === '0:01', 'Countdown rounds up');
ok(countdown(100, 200_000) === '0:00', 'Countdown never goes negative');

const op = (id: number): PlaylistOp => ({
	id,
	kind: 'recover',
	summary: { playlists: [], count: 1 },
	createdAt: 0,
	undone: false,
	undoable: true
});
const applied: RecoverApplied = {
	playlists: [
		{ playlist_id: 'VLA', ops: [op(1), op(2)], done: 3, failed: [[ks[1], 'gone']] },
		{ playlist_id: 'VLB', ops: [], done: 0, failed: [[ks[2], 'cooldown:5']] },
		{ playlist_id: 'VLC', ops: [op(3)], done: 1, failed: [] }
	]
};
const tot = appliedTotals(applied);
ok(tot.ops.map((o) => o.id).join() === '1,2,3', 'Ops flatten in order (announceOps undoes newest first)');
ok(tot.done === 4 && tot.playlists === 2, 'Rows done and playlists edited');
ok(tot.failed.length === 2 && tot.failed[1].key === ks[2] && tot.failed[1].error === 'cooldown:5', 'Failed rows');
ok(appliedTotals({ playlists: [] }).done === 0, 'Nothing applied');

console.log('ok');
