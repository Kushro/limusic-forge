// Self-check for the jobs page's pure logic in `components/jobs/jobs.ts`. Node 22 runs TypeScript
// directly:
//
//     node --experimental-strip-types ui/src/lib/jobs.check.ts
//
// Prints "ok" and exits 0, or throws on the first broken invariant. Not imported by the app, so it
// never reaches the bundle.
import type { BudgetPartition, JobView, PlaylistOp } from './api.ts';
import {
	JOB_STATUSES,
	budgetSegments,
	canMove,
	canPause,
	canPrioritize,
	canResume,
	canRetry,
	canRevert,
	dayOfMonth,
	groupByDay,
	isEnded,
	jobTitle,
	mergeHistory,
	moveId,
	priorityKey,
	progress,
	queueCounts,
	spentFraction,
	statusKey,
	statusTone,
	trackCount,
	undoable,
	untilReset,
	usageBars,
	utcDay
} from './components/jobs/jobs.ts';

function ok(cond: unknown, msg: string): asserts cond {
	if (!cond) throw new Error(`jobs.check: ${msg}`);
}
const same = (a: unknown, b: unknown, msg: string) =>
	ok(JSON.stringify(a) === JSON.stringify(b), `${msg}: ${JSON.stringify(a)} != ${JSON.stringify(b)}`);

function job(id: number, extra: Partial<JobView> = {}): JobView {
	return {
		id,
		kind: 'copy_items',
		op_kind: 'copy',
		status: 'queued',
		priority: 2,
		engine: 'ytdata',
		account_id: 'UC1',
		phase: 1,
		total_phases: 1,
		created_at: '2026-10-06T10:00:00Z',
		started_at: null,
		finished_at: null,
		resume_at: null,
		est_units_total: 100,
		spent_units: 0,
		total_items: 2,
		done_items: 0,
		failed_items: 0,
		skipped_items: 0,
		planned_items: 2,
		retried_items: 0,
		last_error: null,
		summary: { playlists: [{ id: 'PLsrc', title: 'Source' }, { id: 'PLdst', title: 'Mix' }], count: 2 },
		undo_of_job_id: null,
		revert_job_id: null,
		revert_requested: false,
		op_id: null,
		...extra
	};
}
const op = (id: number, createdAt: number, extra: Partial<PlaylistOp> = {}): PlaylistOp => ({
	id,
	kind: 'copy',
	summary: { playlists: [], count: 1 },
	createdAt,
	undone: false,
	undoable: true,
	...extra
});

// --- statuses -------------------------------------------------------------------------------------
same(JOB_STATUSES.length, 11, 'every status');
same(JOB_STATUSES.filter(isEnded), ['completed', 'completed_with_errors', 'failed', 'cancelled'], 'ended ones');
same(statusKey('waiting_quota'), 'jobs.status.waiting_quota', 'status key');
same(
	JOB_STATUSES.map(statusTone),
	['neutral', 'info', 'neutral', 'warning', 'error', 'warning', 'info', 'success', 'warning', 'error', 'neutral'],
	'tones follow PlaylistForge'
);

// --- what may be asked ----------------------------------------------------------------------------
ok(canPause(job(1)) && canPause(job(1, { status: 'waiting_quota' })), 'pause from queued, waiting');
ok(!canPause(job(1, { status: 'paused_user' })) && !canPause(job(1, { status: 'completed' })), 'no pause');
ok(canResume(job(1, { status: 'paused_user' })) && canResume(job(1, { status: 'waiting_auth' })), 'resume');
ok(!canResume(job(1)), 'a queued job has nothing to resume');
ok(canRetry(job(1, { status: 'completed_with_errors', failed_items: 1 })), 'retry with failures');
ok(!canRetry(job(1, { status: 'completed_with_errors', skipped_items: 1 })), 'skipped is not retried');
ok(canPrioritize(job(1)) && !canPrioritize(job(1, { priority: 0 })), 'SYSTEM keeps its priority');
ok(!canPrioritize(job(1, { status: 'failed' })), 'an ended job has no priority to change');
const doneJob = job(1, { status: 'completed', done_items: 2 });
ok(canRevert(doneJob), 'an ended job that did something can be undone');
ok(!canRevert(job(1, { status: 'running', done_items: 2 })), 'not while it runs');
ok(!canRevert({ ...doneJob, revert_job_id: 9 }), 'only once');
ok(!canRevert({ ...doneJob, revert_requested: true }), 'not while one is pending');
ok(!canRevert({ ...doneJob, undo_of_job_id: 3 }), 'an undo is not undone from here');
ok(!canRevert({ ...doneJob, done_items: 0 }), 'nothing done, nothing to undo');
same([0, 1, 2, 3, 9].map(priorityKey), [
	'jobs.priority.system',
	'jobs.priority.high',
	'jobs.priority.normal',
	'jobs.priority.low',
	'jobs.priority.low'
], 'priority keys');

// --- title and progress ---------------------------------------------------------------------------
same(trackCount(job(1, { planned_items: 6, total_items: 13 })), 6, 'a move titles what was asked');
same(trackCount(job(1, { planned_items: 0, total_items: 5 })), 5, 'old rows fall back to the total');
same(jobTitle(job(1)), { key: 'jobs.title.copy', params: { count: 2, playlist: 'Mix' } }, 'copy names the target');
same(
	jobTitle(job(1, { op_kind: 'dedupe', kind: 'remove_items' })),
	{ key: 'jobs.title.dedupe', params: { count: 2, playlist: 'Source' } },
	'an edit names the playlist edited'
);
same(
	jobTitle(job(1, { op_kind: null, kind: 'move_items', summary: null })),
	{ key: 'jobs.title.move_bare', params: { count: 2 } },
	'no playlist known'
);
same(
	jobTitle(job(4, { kind: 'undo', op_kind: 'undo', undo_of_job_id: 3 })),
	{ key: 'jobs.title.undo', params: { id: 3 } },
	'an undo names the job it undoes'
);
same(
	jobTitle(job(5, { kind: 'split_by_channel', op_kind: null })),
	{ key: 'jobs.title.other', params: { kind: 'split_by_channel', id: 5 } },
	'an unknown kind survives'
);
same(progress(job(1, { total_items: 4, done_items: 1, failed_items: 1 })), { settled: 2, total: 4, fraction: 0.5 }, 'progress');
same(progress(job(1, { total_items: 0, status: 'completed' })).fraction, 1, 'an empty ended job is full');
same(progress(job(1, { total_items: 0 })).fraction, 0, 'an empty queued job is empty');

same(
	queueCounts([
		job(1),
		job(2, { status: 'running' }),
		job(3, { status: 'waiting_quota' }),
		job(4, { status: 'paused_network' }),
		job(5, { status: 'waiting_auth' }),
		job(6, { status: 'paused_user' }),
		job(7, { status: 'completed' })
	]),
	{ active: 2, waiting_quota: 2, waiting_auth: 1, paused: 1 },
	'queue counts'
);

// --- reorder --------------------------------------------------------------------------------------
same(moveId([1, 2, 3], 2, 0), [3, 1, 2], 'to the top');
same(moveId([1, 2, 3], 0, 9), [2, 3, 1], 'past the end clamps');
same(moveId([1, 2, 3], 5, 0), [1, 2, 3], 'nothing at that index');
const queue = [job(1, { priority: 1 }), job(2), job(3), job(4, { priority: 0 })];
ok(!canMove(queue, 0, 1), 'a drag never crosses a level');
ok(canMove(queue, 1, 1) && canMove(queue, 2, -1), 'within a level');
ok(!canMove(queue, 2, 1) && !canMove(queue, 0, -1), 'not past an edge or into SYSTEM');

// --- budget bar -----------------------------------------------------------------------------------
const partition: BudgetPartition = {
	daily_units: 10_000,
	spent_total: 73,
	spent_backup: 20,
	spent_jobs: 50,
	spent_other: 3,
	reserve_remaining: 480,
	safety_margin: 300,
	available_for_jobs: 9147,
	next_reset: '2026-10-07T07:00:00Z'
};
const segs = budgetSegments(partition);
same(segs.map((s) => s.key), ['backup', 'jobs', 'other', 'reserve', 'margin', 'available'], 'segment order');
same(segs.map((s) => s.units), [20, 50, 3, 480, 300, 9147], 'segment units');
same(segs[1].percent, 0.5, 'percent of the daily quota');
ok(segs.reduce((n, s) => n + s.percent, 0) <= 100, 'never more than the bar');
const over = budgetSegments({ ...partition, spent_other: 20_000, available_for_jobs: -5 });
same(over.find((s) => s.key === 'available')?.units, 0, 'a negative is nothing');
ok(Math.abs(over.reduce((n, s) => n + s.percent, 0) - 100) < 1e-9, 'an overspent day scales to the bar');
const now = Date.parse('2026-10-06T19:30:00Z');
same(untilReset('2026-10-07T07:00:00Z', now), { hours: 11, minutes: 30 }, 'countdown');
same(untilReset('2026-10-06T07:00:00Z', now), { hours: 0, minutes: 0 }, 'never negative');
same(untilReset('garbage', now), { hours: 0, minutes: 0 }, 'an unreadable date');
same([spentFraction(2500, 10_000), spentFraction(20_000, 10_000), spentFraction(5, 0)], [0.25, 1, 0], 'spent share');

// --- 14-day usage ---------------------------------------------------------------------------------
const bars = usageBars(
	[
		{ date: '2026-10-04', units: 0 },
		{ date: '2026-10-05', units: 5000 },
		{ date: '2026-10-06', units: 2500 }
	],
	10_000
);
same(bars.map((b) => b.percent), [0, 50, 25], 'scaled to the quota when no day reached it');
same(bars.map((b) => b.today), [false, false, true], 'the last bar is today');
same(bars[1].ofQuota, 0.5, 'share of the quota');
same(usageBars([{ date: '2026-10-06', units: 20_000 }], 10_000)[0].percent, 100, 'a day over quota fills');
same(usageBars([], 10_000), [], 'no days, no bars');
same(dayOfMonth('2026-10-06'), 6, 'day of month');

// --- history --------------------------------------------------------------------------------------
const DAY = 86_400;
const T = Date.parse('2026-10-06T12:00:00Z') / 1000;
const ended = job(10, { status: 'completed', done_items: 2, finished_at: '2026-10-06T12:00:00Z', op_id: 7 });
const failed = job(11, { status: 'failed', done_items: 0, finished_at: '2026-10-05T09:00:00Z' });
const history = mergeHistory(
	[ended, failed, job(12)],
	[op(7, T, { kind: 'move' }), op(8, T + 60, { kind: 'reorder', undoable: false }), op(9, T - 2 * DAY)]
);
same(
	history.map((e) => (e.type === 'job' ? `job${e.job.id}` : `op${e.op.id}`)),
	['op8', 'job10', 'job11', 'op9'],
	'newest first, a job with its own entry listed once, active jobs left out'
);
const first = history[1];
ok(first.type === 'job' && first.op?.id === 7, 'the job carries its journal entry');
same(history.map(undoable), [false, true, false, true], 'undo follows the entry, else the job');
same(
	groupByDay(history, utcDay).map((g) => [g.day, g.items.length]),
	[
		['2026-10-06', 2],
		['2026-10-05', 1],
		['2026-10-04', 1]
	],
	'grouped by day, newest first'
);
same(groupByDay([], utcDay), [], 'no entries, no days');
same(mergeHistory([], []), [], 'empty history');

console.log('ok');
