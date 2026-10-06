// Download state per video, for the rows on screen and the menus over them.
//
// The `downloads` table is the truth (`download/` in Rust); this is a cache of `downloads_for` for
// the ids the UI has asked about, kept current by `downloads-changed` (refetch those ids, or every
// cached one when the event names none) and `download-progress` (the one run going now). A row
// calls `track` with its id; ids arriving together go out as one batched request, and an id is
// fetched once until an event says it moved. Nothing here is polled.
//
// The listeners start with the first `track`/`enqueue`, not at import: a page with no track rows
// costs nothing.
import { SvelteMap } from 'svelte/reactivity';
import * as api from './api';
import type { DownloadFormat, DownloadProgress, DownloadRow, DownloadSong } from './api';
import { t } from './i18n.svelte';
import { toast } from './player.svelte';

/** What a row shows. `none` also covers "not fetched yet". */
export type DownloadState = 'none' | 'queued' | 'running' | 'available' | 'error' | 'missing';

/** Which of a video's rows speaks for it when it has several (audio and video): the one that is
 *  moving first, then a file that is there, then a problem. */
const PRIORITY = ['running', 'queued', 'available', 'error', 'missing'] as const;
/** Ids per `downloads_for` call: the backend checks each one's file on disk. */
const CHUNK = 400;
/** How long `track` calls gather before one request goes out (a list mounting its rows). */
const BATCH_MS = 40;

const rows = new SvelteMap<string, DownloadRow[]>();
const live = $state<{ progress: DownloadProgress | null }>({ progress: null });

const pending = new Set<string>();
/** Latest request per id: an older answer landing after a newer one is dropped. */
const generation = new Map<string, number>();
let timer: ReturnType<typeof setTimeout> | null = null;
let started = false;

function start() {
	if (started) return;
	started = true;
	api.onDownloadProgress((p) => (live.progress = p)).catch(() => {});
	api.onDownloadsChanged((c) => {
		// Only what is cached: an id nobody has asked about is fetched when something shows it.
		const ids = c.video_ids.length ? c.video_ids.filter((id) => rows.has(id)) : [...rows.keys()];
		if (ids.length) void fetchAll(ids);
		const o = c.outcome;
		if (o?.status === 'error')
			toast.error(t('downloads.failed', { title: o.title ?? o.video_id, error: o.error ?? '' }));
		else if (o?.status === 'tools_missing') toast.error(t('downloads.tools_missing'));
	}).catch(() => {});
	api
		.downloadActive()
		.then((p) => {
			// An event that came in first is newer than this answer.
			if (live.progress === null) live.progress = p;
		})
		.catch(() => {});
}

/** Ask for these videos' download state. Cheap to call on every mount: cached ids are skipped. */
export function track(ids: Iterable<string>) {
	start();
	for (const id of ids) {
		if (!id || api.isLocalId(id) || rows.has(id) || generation.has(id)) continue;
		pending.add(id);
	}
	if (pending.size && !timer) timer = setTimeout(flush, BATCH_MS);
}

function flush() {
	timer = null;
	const ids = [...pending];
	pending.clear();
	void fetchAll(ids);
}

async function fetchAll(ids: string[]) {
	for (let i = 0; i < ids.length; i += CHUNK) await fetchSome(ids.slice(i, i + CHUNK));
}

async function fetchSome(ids: string[]) {
	const asked = new Map<string, number>();
	for (const id of ids) {
		const n = (generation.get(id) ?? 0) + 1;
		generation.set(id, n);
		asked.set(id, n);
	}
	let got: Record<string, DownloadRow[]>;
	try {
		got = await api.downloadsFor(ids);
	} catch {
		// Unknown again, so the next `track` retries rather than showing "none" for good.
		for (const id of ids) if (generation.get(id) === asked.get(id) && !rows.has(id)) generation.delete(id);
		return;
	}
	for (const id of ids) {
		if (generation.get(id) !== asked.get(id)) continue;
		const next = got[id] ?? [];
		const prev = rows.get(id);
		// An empty answer for an id already known empty changes nothing on screen.
		if (prev && !prev.length && !next.length) continue;
		rows.set(id, next);
	}
}

/** The video's rows as last fetched (empty when it has none, or before the first fetch). */
export function rowsOf(videoId: string): DownloadRow[] {
	return rows.get(videoId) ?? [];
}

/** One state for the video, or for one of its formats. */
export function statusOf(videoId: string, format?: DownloadFormat): DownloadState {
	const p = live.progress;
	if (p && p.video_id === videoId && (!format || p.format === format)) return 'running';
	const list = rows.get(videoId);
	if (!list?.length) return 'none';
	const of = format ? list.filter((r) => r.format === format) : list;
	return PRIORITY.find((s) => of.some((r) => r.status === s)) ?? 'none';
}

/** Whether the video has a downloaded file (either format). The `downloaded` facet's test. */
export function isDownloaded(videoId: string): boolean {
	return rows.get(videoId)?.some((r) => r.status === 'available') ?? false;
}

/** The run going now when it is this video's; null otherwise. */
export function progressOf(videoId: string): DownloadProgress | null {
	const p = live.progress;
	return p && p.video_id === videoId ? p : null;
}

/** The run going now, whichever video it is. */
export function activeDownload(): DownloadProgress | null {
	return live.progress;
}

/**
 * Queue songs (`format`: the `downloads.default_format` setting when absent) and say how it went.
 * Local files are left out here as well as in the backend, so the count the toast gives is real.
 * Never throws: a failure toasts.
 */
export async function enqueue(
	songs: DownloadSong[],
	format?: DownloadFormat,
	redownload = false
): Promise<api.EnqueueResult | null> {
	start();
	const list = songs
		.filter((s) => s.video_id && !api.isLocalId(s.video_id))
		.map(({ video_id, title, artists, duration }) => ({ video_id, title, artists, duration }));
	if (!list.length) {
		toast(t('downloads.nothing'));
		return null;
	}
	// Cached from here on, so the rows show "queued" when the change event lands.
	track(list.map((s) => s.video_id));
	try {
		const r = await api.downloadEnqueue(list, format, redownload);
		// Songs whose id is not a YouTube video's are left out; the toast says how many.
		const invalid = r.invalid ? t('downloads.invalid_note', { count: r.invalid }) : '';
		const note = (main: string) => (invalid ? `${main} · ${invalid}` : main);
		if (r.queued === 0 && r.already === 0 && invalid) toast.error(invalid);
		else if (r.queued === 0) toast(note(t('downloads.already', { count: r.already })));
		else if (r.already === 0)
			toast.success(
				note(r.queued === 1 ? t('downloads.queued_one') : t('downloads.queued', { count: r.queued }))
			);
		else toast.success(note(t('downloads.queued_some', { count: r.queued, already: r.already })));
		return r;
	} catch (e) {
		toast.error(t('downloads.enqueue_failed', { error: String(e) }));
		return null;
	}
}

/** Forget the video's row in that format (stopping it if it runs). Never the file. */
export async function remove(videoId: string, format: DownloadFormat) {
	try {
		if (await api.downloadRemove(videoId, format)) toast.success(t('downloads.removed'));
	} catch (e) {
		toast.error(String(e));
	}
}

/** An errored or missing row back into the queue. */
export async function retry(videoId: string, format: DownloadFormat) {
	try {
		if (await api.downloadRetry(videoId, format)) toast.success(t('downloads.queued_one'));
	} catch (e) {
		toast.error(String(e));
	}
}

/** Stop the run going now (its row goes; the partial file stays for a later resume). */
export async function cancel() {
	try {
		await api.downloadCancel();
	} catch (e) {
		toast.error(String(e));
	}
}

/** The download folder in the file manager. */
export async function showFolder() {
	try {
		await api.openDownloadsDir();
	} catch (e) {
		toast.error(String(e));
	}
}
