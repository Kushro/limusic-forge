// The alerts page's logic (routes/alerts): filtering the monitor's alerts, grouping them by day and
// counting them by kind, telling the resolved `unavailable` ones from the open ones, plus the
// one-line sums the timeline shows. Pure and rune-free on purpose —
// `alerts.check.ts` runs it under plain node (`node --experimental-strip-types`).
import type { AlertKind, PlaylistAlert, TimelineEntry } from './api';

/** Every kind, in the order the page lists them (and the filter offers them). */
export const ALERT_KINDS: readonly AlertKind[] = [
	'removed',
	'unavailable',
	'added',
	'moved',
	'restored'
];

export const isAlertKind = (s: string | null | undefined): s is AlertKind =>
	!!s && (ALERT_KINDS as readonly string[]).includes(s);

export type AlertFilter = {
	/** The kinds to keep; empty or missing keeps them all. */
	kinds?: readonly AlertKind[];
	/** One playlist's alerts only; null or missing is every playlist. */
	playlist?: string | null;
	/** Only the ones not seen yet. */
	unseenOnly?: boolean;
	/** Keep the ones dismissed from Library ▸ In your playlists (the page keeps them by default:
	 *  dismissing hides a row there, it does not unmake what happened). */
	dismissed?: boolean;
};

/** The alerts that pass `f`, in the order given (newest first, as the backend answers). */
export function filterAlerts(alerts: readonly PlaylistAlert[], f: AlertFilter = {}): PlaylistAlert[] {
	const kinds = f.kinds?.length ? new Set(f.kinds) : null;
	const keepDismissed = f.dismissed ?? true;
	return alerts.filter(
		(a) =>
			(!kinds || kinds.has(a.kind)) &&
			(!f.playlist || a.playlist_id === f.playlist) &&
			(!f.unseenOnly || !a.seen) &&
			(keepDismissed || !a.dismissed)
	);
}

/** How many of each kind, every kind present (zero included). */
export function countByKind(alerts: readonly PlaylistAlert[]): Record<AlertKind, number> {
	const out = Object.fromEntries(ALERT_KINDS.map((k) => [k, 0])) as Record<AlertKind, number>;
	for (const a of alerts) if (a.kind in out) out[a.kind]++;
	return out;
}

/** What tells one alert row from another: its id, or its fields on a row without one. */
export const alertKey = (a: PlaylistAlert): string =>
	a.id !== undefined ? `#${a.id}` : `${a.playlist_id}\u001f${a.video_id}\u001f${a.kind}\u001f${a.at}`;

/** The keys (`alertKey`) of the `unavailable` alerts among `alerts` that a later `restored` of the
 *  same playlist and track answers. The backend's `EXISTS` (`db::ALERT_ROW_COLUMNS`) over what is
 *  loaded: later is a greater `at`, or the same one and a greater id. Only a later one, so a track
 *  that came back and went again is open once more. */
export function resolvedKeys(alerts: readonly PlaylistAlert[]): Set<string> {
	const pair = (a: PlaylistAlert) => `${a.playlist_id}\u001f${a.video_id}`;
	const after = (x: { at: number; id?: number }, y: { at: number; id?: number }) =>
		x.at > y.at || (x.at === y.at && (x.id ?? -1) > (y.id ?? -1));
	// The latest restore of each track is the only one that matters: any later one is later still.
	const latest = new Map<string, PlaylistAlert>();
	for (const a of alerts) {
		if (a.kind !== 'restored') continue;
		const l = latest.get(pair(a));
		if (!l || after(a, l)) latest.set(pair(a), a);
	}
	const out = new Set<string>();
	for (const a of alerts) {
		const r = a.kind === 'unavailable' ? latest.get(pair(a)) : undefined;
		if (r && after(r, a)) out.add(alertKey(a));
	}
	return out;
}

/** Whether `a` is an `unavailable` alert since answered. The backend's flag where the row has one
 *  (it sees every row, not just the loaded pages); otherwise `keys` from `resolvedKeys`. */
export const isResolved = (a: PlaylistAlert, keys?: ReadonlySet<string>): boolean =>
	a.kind === 'unavailable' && (a.resolved ?? keys?.has(alertKey(a)) ?? false);

/** The `unavailable` alerts split into the ones still open and the ones resolved. */
export function unavailableCounts(
	alerts: readonly PlaylistAlert[],
	keys?: ReadonlySet<string>
): { open: number; resolved: number } {
	let open = 0;
	let resolved = 0;
	for (const a of alerts) {
		if (a.kind !== 'unavailable') continue;
		if (isResolved(a, keys)) resolved++;
		else open++;
	}
	return { open, resolved };
}

/** The playlists alerts were filed for, most alerts first (ties by id, so the order is stable). */
export function alertPlaylists(alerts: readonly PlaylistAlert[]): { id: string; count: number }[] {
	const n = new Map<string, number>();
	for (const a of alerts) n.set(a.playlist_id, (n.get(a.playlist_id) ?? 0) + 1);
	return [...n]
		.map(([id, count]) => ({ id, count }))
		.sort((x, y) => y.count - x.count || (x.id < y.id ? -1 : x.id > y.id ? 1 : 0));
}

/** The ids of the alerts not seen yet (the ones "Mark as seen" has anything to do for). */
export const unseenIds = (alerts: readonly PlaylistAlert[]): number[] =>
	alerts.flatMap((a) => (!a.seen && a.id !== undefined ? [a.id] : []));

const pad = (n: number) => String(n).padStart(2, '0');

/** `at` (unix seconds) as the local calendar day it fell on, `YYYY-MM-DD`. */
export function localDay(at: number): string {
	const d = new Date(at * 1000);
	return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** The same in UTC: what the checks use, so they pass in any time zone. */
export function utcDay(at: number): string {
	const d = new Date(at * 1000);
	return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}`;
}

export type DayGroup<T> = { day: string; items: T[] };

/** Consecutive runs of the same day, in the order given: newest-first in, newest day first out.
 *  `dayOf` is the calendar the days are read in (local by default). */
export function groupByDay<T extends { at: number }>(
	items: readonly T[],
	dayOf: (at: number) => string = localDay
): DayGroup<T>[] {
	const out: DayGroup<T>[] = [];
	for (const it of items) {
		const day = dayOf(it.at);
		const last = out.at(-1);
		if (last && last.day === day) last.items.push(it);
		else out.push({ day, items: [it] });
	}
	return out;
}

/** Whole days from `day` to `today` (both `YYYY-MM-DD`): 0 today, 1 yesterday. */
export function daysAgo(day: string, today: string): number {
	const t = (s: string) => Date.UTC(+s.slice(0, 4), +s.slice(5, 7) - 1, +s.slice(8, 10));
	return Math.round((t(today) - t(day)) / 86_400_000);
}

/** A timeline entry's sums as `+N −N ~N` and the rest, the kinds with nothing left out. */
export function timelineSums(e: TimelineEntry): { kind: AlertKind; n: number; sign: string }[] {
	const parts: { kind: AlertKind; n: number; sign: string }[] = [
		{ kind: 'added', n: e.added, sign: '+' },
		{ kind: 'removed', n: e.removed, sign: '−' },
		{ kind: 'moved', n: e.moved, sign: '~' },
		{ kind: 'unavailable', n: e.unavailable, sign: '!' },
		{ kind: 'restored', n: e.restored, sign: '↺' }
	];
	return parts.filter((p) => p.n > 0);
}

/** Where a song plays on YouTube Music. */
export const ytmSongUrl = (videoId: string) =>
	`https://music.youtube.com/watch?v=${encodeURIComponent(videoId)}`;
