// Tools ▸ Recover tracks: the assistant's state for as long as the app runs (F4.7), so leaving the
// page halfway and coming back finds the same step, filters and selection. The candidates and the
// rows' progress are Rust's (`playlist_tools/recover.rs`); this caches the candidates and keeps
// what is only the page's: filters, selection, step. The pure rules are in recover.ts.
import { goto } from '$app/navigation';
import * as api from './api';
import { t, type TranslationKey } from './i18n.svelte';
import { clockTime, cooldownUntil } from './import.svelte';
import {
	clickSelect,
	emptyFilter,
	filterCandidates,
	orderedSelection,
	retainKeys,
	selectFiltered,
	type RecoverFilter
} from './recover';

/** The assistant's steps, in order. Each later one works on `selectedKeys()`. */
export const RECOVER_STEPS = ['pick', 'titles', 'search', 'review', 'apply'] as const;
export type RecoverStep = (typeof RECOVER_STEPS)[number];

export const rec = $state({
	step: 'pick' as RecoverStep,
	/** Null until the first load; the last list read after that, even while a reload runs. */
	candidates: null as api.RecoverCandidate[] | null,
	/** Whether `candidates` was read with the dismissed ones (`filter.includeDismissed`). */
	loadedDismissed: false,
	loading: false,
	error: null as string | null,
	filter: emptyFilter() as RecoverFilter,
	/** Replaced on every change, never mutated: a Set in `$state` is not itself reactive. */
	selected: new Set<string>() as Set<string>,
	/** Where the next Shift+click range starts: the row clicked last. */
	anchor: null as string | null,
	/** A key to select and scroll to on the next load (from an alert's "Recover with the
	 *  assistant"). Cleared once applied. */
	preselect: null as string | null,
	/** The key to bring into view once, after a preselect landed. */
	focus: null as string | null,
	/** The preselected key was not among the candidates (resolved or back meanwhile). */
	preselectMissing: false,
	/** The last `recover-progress` (titles, search, apply). The later steps keep it current. */
	snapshot: null as api.RecoverSnapshot | null,
	/** The phase of the last run this side started (`titles`, `searching`, `applying`): a `done`
	 *  snapshot does not say which step finished, this does. Null once its end was acted on. */
	ran: null as api.RecoverPhase | null
});

let tracking = false;

/** Follow `recover-progress` into `rec.snapshot`, once for as long as the app runs (as imports do):
 *  Rust has no status call, so a run that ends while the page is closed must still land here. */
export function trackRecover() {
	if (tracking) return;
	tracking = true;
	void api.onRecoverProgress((s) => (rec.snapshot = s));
}

/** Start a background run (`recoverTitles`, `recoverSearch`) as `phase`. The events that follow
 *  are newer than the snapshot the call answers, so that one only lands if none came first. */
export async function startRun(phase: api.RecoverPhase, run: () => Promise<api.RecoverSnapshot>) {
	trackRecover();
	rec.snapshot = null;
	rec.ran = phase;
	try {
		const s = await run();
		if (!rec.snapshot) rec.snapshot = s;
	} catch (e) {
		rec.ran = null;
		throw e;
	}
}

/** A rejection from a recover command or a step's `message`, in the user's language when it is
 *  one of ours (`busy`, `cooldown:<until>`, `gone`, `invalid_id`, `wayback_rate_limited`). */
export function recoverError(e: unknown): string {
	const raw = String(e ?? '');
	const until = cooldownUntil(raw);
	const key = `recover.errors.${until ? 'cooldown' : raw}` as TranslationKey;
	const text = t(key, until ? { time: clockTime(until) } : undefined);
	return text === key ? raw : text;
}

/** The candidates the filters show, in list order. */
export const shownCandidates = () => filterCandidates(rec.candidates ?? [], rec.filter);
/** The selection in list order: what the later steps hand to `recoverTitles`/`recoverSearch`/
 *  `recoverApply`. */
export const selectedKeys = () => orderedSelection(rec.selected, rec.candidates ?? []);
/** The selected candidates, in list order. */
export const selectedCandidates = () => (rec.candidates ?? []).filter((c) => rec.selected.has(c.key));

/** Read the candidates again (on open, after "Include dismissed" flips, or after an apply). Keeps
 *  the selection that still applies, then lands a pending preselect. */
export async function loadCandidates() {
	if (rec.loading) return;
	rec.loading = true;
	rec.error = null;
	const dismissed = rec.filter.includeDismissed;
	try {
		const list = await api.recoverCandidates(dismissed);
		rec.candidates = list;
		rec.loadedDismissed = dismissed;
		rec.selected = retainKeys(rec.selected, list);
		if (rec.anchor && !rec.selected.has(rec.anchor)) rec.anchor = null;
		landPreselect();
	} catch (e) {
		rec.error = String(e ?? '');
	} finally {
		rec.loading = false;
	}
	// "Include dismissed" turned on while that read ran without them.
	if (!rec.error && rec.filter.includeDismissed && !rec.loadedDismissed) void loadCandidates();
}

/** Select the preselected candidate and clear any filter that would hide it. */
function landPreselect() {
	const key = rec.preselect;
	if (!key || !rec.candidates) return;
	rec.preselect = null;
	const c = rec.candidates.find((c) => c.key === key);
	rec.preselectMissing = !c;
	if (!c) return;
	if (!filterCandidates([c], rec.filter).length) {
		rec.filter = { ...emptyFilter(), includeDismissed: rec.filter.includeDismissed };
	}
	rec.selected = new Set([...rec.selected, key]);
	rec.anchor = key;
	rec.focus = key;
	rec.step = 'pick';
}

/** A row's checkbox or the row itself was clicked; Shift adds the range from the last one. */
export function clickRow(key: string, shift: boolean) {
	const keys = shownCandidates().map((c) => c.key);
	const r = clickSelect(rec.selected, keys, rec.anchor, key, shift);
	rec.selected = r.selected;
	rec.anchor = r.anchor;
}

/** Every candidate listed (the dismissed switch applies, the other filters don't). */
export function selectAll() {
	rec.selected = selectFiltered(
		rec.selected,
		filterCandidates(rec.candidates ?? [], { ...emptyFilter(), includeDismissed: rec.filter.includeDismissed })
	);
}

export function selectShown() {
	rec.selected = selectFiltered(rec.selected, shownCandidates());
}

export function clearSelection() {
	rec.selected = new Set();
	rec.anchor = null;
}

export function setFilter(patch: Partial<RecoverFilter>) {
	rec.filter = { ...rec.filter, ...patch };
	// The dismissed ones are only read when asked for: a list read without them can't show them.
	if (rec.filter.includeDismissed && !rec.loadedDismissed) void loadCandidates();
}

export function goToStep(step: RecoverStep) {
	rec.step = step;
}

/** The step after this one (`pick` → `titles` → ...), or the same at the end. */
export function nextStep(step: RecoverStep = rec.step): RecoverStep {
	const i = RECOVER_STEPS.indexOf(step);
	return RECOVER_STEPS[Math.min(i + 1, RECOVER_STEPS.length - 1)];
}

/** Open the assistant, with this candidate (`api.recoverKey`) selected when given: an alert's
 *  "Recover with the assistant". */
export function openRecover(key?: string) {
	if (key) {
		rec.preselect = key;
		rec.preselectMissing = false;
	}
	return goto('/tools/recover');
}

/** Start over: no selection, back on the first step, Rust's session forgotten. */
export async function resetRecover() {
	await api.recoverReset().catch(() => {});
	rec.step = 'pick';
	rec.selected = new Set();
	rec.anchor = null;
	rec.snapshot = null;
	rec.ran = null;
	rec.candidates = null;
	await loadCandidates();
}
