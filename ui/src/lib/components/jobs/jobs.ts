// The jobs page's logic (routes/jobs): status labels and tones, what each job may be asked to do,
// how a job is titled, today's budget split into bar segments, the 14-day usage bars, and the
// history (ended jobs and undo-journal entries) grouped by day. Pure and rune-free on purpose —
// `lib/jobs.check.ts` runs it under plain node (`node --experimental-strip-types`). Wording lives in
// en.json: these answer keys and numbers, never sentences.
import type {
	BudgetPartition,
	DailyUsage,
	JobStatus,
	JobView,
	PlaylistOp
} from '../../api';

/** Every status, in the order the backend's `JobStatus::ALL` lists them. */
export const JOB_STATUSES: readonly JobStatus[] = [
	'queued',
	'running',
	'paused_user',
	'waiting_quota',
	'waiting_auth',
	'paused_network',
	'verifying',
	'completed',
	'completed_with_errors',
	'failed',
	'cancelled'
];

const ENDED: readonly JobStatus[] = ['completed', 'completed_with_errors', 'failed', 'cancelled'];

/** Done for good: the job will not change state again on its own. */
export const isEnded = (s: JobStatus) => ENDED.includes(s);

export const statusKey = (s: JobStatus) => `jobs.status.${s}` as const;
/** The longer sentence under the chip. */
export const statusHintKey = (s: JobStatus) => `jobs.status_hint.${s}` as const;

/** The chip's colour bucket (PlaylistForge's `status_chip_tone`). */
export type Tone = 'neutral' | 'info' | 'warning' | 'error' | 'success';
export function statusTone(s: JobStatus): Tone {
	switch (s) {
		case 'running':
		case 'verifying':
			return 'info';
		case 'waiting_quota':
		case 'paused_network':
		case 'completed_with_errors':
			return 'warning';
		case 'waiting_auth':
		case 'failed':
			return 'error';
		case 'completed':
			return 'success';
		default:
			return 'neutral';
	}
}

// --- what a job may be asked ----------------------------------------------------------------------
// The same transitions `jobs/repo.rs` accepts, so a button is only there when it would do something.

export const canPause = (j: JobView) =>
	(['queued', 'running', 'waiting_quota', 'paused_network'] as JobStatus[]).includes(j.status);
export const canResume = (j: JobView) => j.status === 'paused_user' || j.status === 'waiting_auth';
export const canCancel = (j: JobView) => !isEnded(j.status);
export const canRetry = (j: JobView) => j.failed_items > 0;
/** SYSTEM (0) is the app's own (an undo): its priority is not the user's to change. */
export const canPrioritize = (j: JobView) => !isEnded(j.status) && j.priority > 0;
/** Undoing an ended job that did something, once: not an undo itself, not already reverted. */
export const canRevert = (j: JobView) =>
	isEnded(j.status) &&
	j.done_items > 0 &&
	j.undo_of_job_id === null &&
	j.revert_job_id === null &&
	!j.revert_requested;

export const PRIORITIES = [1, 2, 3] as const;
export const priorityKey = (p: number) =>
	p <= 0
		? ('jobs.priority.system' as const)
		: p === 1
			? ('jobs.priority.high' as const)
			: p >= 3
				? ('jobs.priority.low' as const)
				: ('jobs.priority.normal' as const);

// --- title and progress ---------------------------------------------------------------------------

/** What the user asked for: the first phase's count, not `total_items`, which grows as a move
 *  appends its verify and delete phases (PlaylistForge's "Move 13 videos" for a 6-track move). */
export const trackCount = (j: JobView) =>
	j.planned_items > 0 ? j.planned_items : Math.max(0, j.total_items);

export type TitleKind = 'copy' | 'move' | 'remove' | 'dedupe' | 'reorder' | 'create' | 'rename' | 'undo' | 'other';

export function titleKind(j: JobView): TitleKind {
	if (j.kind === 'undo' || j.undo_of_job_id !== null) return 'undo';
	switch (j.op_kind ?? j.kind) {
		case 'copy':
		case 'copy_items':
			return 'copy';
		case 'move':
		case 'move_items':
			return 'move';
		case 'remove':
		case 'remove_items':
			return 'remove';
		case 'dedupe':
			return 'dedupe';
		case 'reorder':
			return 'reorder';
		case 'create_playlist':
			return 'create';
		case 'rename_playlist':
			return 'rename';
		default:
			return 'other';
	}
}

/** The playlist a title names: where tracks went for a copy or a move (the summary lists the
 *  source first), the one edited otherwise. Null when the job named none. */
export function titlePlaylist(j: JobView): string | null {
	const pls = j.summary?.playlists ?? [];
	if (!pls.length) return null;
	const kind = titleKind(j);
	const p = kind === 'copy' || kind === 'move' ? pls[pls.length - 1] : pls[0];
	return p.title || p.id || null;
}

type Countable = Exclude<TitleKind, 'undo' | 'other'>;
export type TitleKey =
	| 'jobs.title.undo'
	| 'jobs.title.other'
	| `jobs.title.${Countable}`
	| `jobs.title.${Countable}_bare`;

/** The title's key and its parameters: `jobs.title.<kind>` with a playlist, `<kind>_bare` without. */
export function jobTitle(j: JobView): { key: TitleKey; params: Record<string, string | number> } {
	const kind = titleKind(j);
	const playlist = titlePlaylist(j);
	const count = trackCount(j);
	if (kind === 'undo') return { key: 'jobs.title.undo', params: { id: j.undo_of_job_id ?? j.id } };
	if (kind === 'other') return { key: 'jobs.title.other', params: { kind: j.op_kind ?? j.kind, id: j.id } };
	if (playlist === null) return { key: `jobs.title.${kind}_bare`, params: { count } };
	return { key: `jobs.title.${kind}`, params: { count, playlist } };
}

/** How far along: items that reached an end (done, failed or skipped) over every item so far. */
export function progress(j: JobView): { settled: number; total: number; fraction: number } {
	const total = Math.max(0, j.total_items);
	const settled = Math.min(total, j.done_items + j.failed_items + j.skipped_items);
	return { settled, total, fraction: total > 0 ? settled / total : isEnded(j.status) ? 1 : 0 };
}

/** The queue's one-line summary (PlaylistForge's `queue_summary_line`): how many run or wait, and
 *  why the rest are stopped. Zero buckets are left out by the page. */
export function queueCounts(jobs: readonly JobView[]) {
	const c = { active: 0, waiting_quota: 0, waiting_auth: 0, paused: 0 };
	for (const j of jobs) {
		if (j.status === 'queued' || j.status === 'running' || j.status === 'verifying') c.active++;
		else if (j.status === 'waiting_quota' || j.status === 'paused_network') c.waiting_quota++;
		else if (j.status === 'waiting_auth') c.waiting_auth++;
		else if (j.status === 'paused_user') c.paused++;
	}
	return c;
}

/** `ids` with the one at `from` moved to `to` (both clamped): the queue's up/down buttons. */
export function moveId(ids: readonly number[], from: number, to: number): number[] {
	const out = [...ids];
	if (from < 0 || from >= out.length) return out;
	const target = Math.max(0, Math.min(out.length - 1, to));
	const [id] = out.splice(from, 1);
	out.splice(target, 0, id);
	return out;
}

/** Whether moving the job at `index` by `delta` stays within its priority level (the backend never
 *  lets a drag change a level). `jobs` in run order. */
export function canMove(jobs: readonly JobView[], index: number, delta: -1 | 1): boolean {
	const a = jobs[index];
	const b = jobs[index + delta];
	return !!a && !!b && a.priority === b.priority && a.priority > 0;
}

// --- budget bar -----------------------------------------------------------------------------------

export type SegmentKey = 'backup' | 'jobs' | 'other' | 'reserve' | 'margin' | 'available';
export type Segment = { key: SegmentKey; units: number; percent: number };

/** Today's partition as the bar's segments, in drawing order (PlaylistForge's budget gauge): what
 *  backups, jobs and the rest spent, the reserve still kept, the margin, what jobs may still spend.
 *  Percent of the daily quota; scaled down together if they add up to more than the bar. */
export function budgetSegments(p: BudgetPartition): Segment[] {
	const raw: [SegmentKey, number][] = [
		['backup', p.spent_backup],
		['jobs', p.spent_jobs],
		['other', p.spent_other],
		['reserve', p.reserve_remaining],
		['margin', p.safety_margin],
		['available', p.available_for_jobs]
	];
	const units = raw.map(([, u]) => Math.max(0, u));
	const sum = units.reduce((a, b) => a + b, 0);
	const whole = Math.max(1, p.daily_units, sum);
	return raw.map(([key], i) => ({ key, units: units[i], percent: (units[i] / whole) * 100 }));
}

/** Hours and minutes until `nextReset` (never negative). */
export function untilReset(nextReset: string, nowMs: number): { hours: number; minutes: number } {
	const ms = Date.parse(nextReset);
	const mins = Number.isFinite(ms) ? Math.max(0, Math.floor((ms - nowMs) / 60_000)) : 0;
	return { hours: Math.floor(mins / 60), minutes: mins % 60 };
}

/** The share of today's quota spent, 0..1. */
export const spentFraction = (spent: number, daily: number) =>
	daily > 0 ? Math.min(1, Math.max(0, spent / daily)) : 0;

// --- 14-day usage ---------------------------------------------------------------------------------

export type UsageBar = DailyUsage & { percent: number; ofQuota: number; today: boolean };

/** Each day's bar height (percent of the busiest day, or of the quota if no day reached it, so a
 *  quiet fortnight does not look full) and its share of the quota. The last day is today. */
export function usageBars(history: readonly DailyUsage[], daily: number): UsageBar[] {
	const peak = Math.max(1, ...history.map((d) => d.units));
	const scale = Math.max(peak, daily > 0 ? daily : 1);
	return history.map((d, i) => ({
		...d,
		percent: (Math.max(0, d.units) / scale) * 100,
		ofQuota: daily > 0 ? d.units / daily : 0,
		today: i === history.length - 1
	}));
}

/** The day of the month of a `YYYY-MM-DD` date, for the bar's label. */
export const dayOfMonth = (date: string) => Number(date.slice(8, 10)) || 0;

// --- history --------------------------------------------------------------------------------------

/** One history row: an ended job (with its undo-journal entry when it has one), or a journal entry
 *  of an edit that never went through the queue. `at` is epoch seconds. */
export type HistoryEntry =
	| { type: 'job'; at: number; job: JobView; op: PlaylistOp | null }
	| { type: 'op'; at: number; op: PlaylistOp };

const seconds = (iso: string | null) => {
	const ms = iso ? Date.parse(iso) : NaN;
	return Number.isFinite(ms) ? Math.floor(ms / 1000) : 0;
};

/** Ended jobs and journal entries in one list, newest first. A job's own entry (`op_id`) rides on
 *  the job's row instead of appearing twice. */
export function mergeHistory(jobs: readonly JobView[], ops: readonly PlaylistOp[]): HistoryEntry[] {
	const byId = new Map(ops.map((o) => [o.id, o]));
	const claimed = new Set<number>();
	const out: HistoryEntry[] = [];
	for (const job of jobs) {
		if (!isEnded(job.status)) continue;
		const op = job.op_id !== null ? (byId.get(job.op_id) ?? null) : null;
		if (op) claimed.add(op.id);
		out.push({ type: 'job', at: seconds(job.finished_at ?? job.created_at), job, op });
	}
	for (const op of ops) if (!claimed.has(op.id)) out.push({ type: 'op', at: op.createdAt, op });
	// Newest first; a tie keeps jobs before entries and otherwise the input order (stable sort).
	return out.sort((a, b) => b.at - a.at);
}

/** `entries` (newest first) grouped by `dayOf(at)`, the groups newest first. */
export function groupByDay<T extends { at: number }>(
	entries: readonly T[],
	dayOf: (at: number) => string
): { day: string; items: T[] }[] {
	const groups: { day: string; items: T[] }[] = [];
	for (const e of entries) {
		const day = dayOf(e.at);
		const last = groups[groups.length - 1];
		if (last && last.day === day) last.items.push(e);
		else groups.push({ day, items: [e] });
	}
	return groups;
}

/** Whether the row offers Undo: the journal entry says it still can be, or (a job not journaled)
 *  the job can be reverted. */
export function undoable(e: HistoryEntry): boolean {
	if (e.type === 'op') return e.op.undoable;
	return e.op ? e.op.undoable : canRevert(e.job);
}

/** The UTC calendar day of epoch seconds (`YYYY-MM-DD`), for tests; the page uses the local day. */
export const utcDay = (at: number) => new Date(at * 1000).toISOString().slice(0, 10);
