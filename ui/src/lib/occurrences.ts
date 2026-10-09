// The "+N" dialog's logic (OccurrencesDialog.svelte): which of a song's copies an action touches,
// and how a picked copy is found again in a fresh read of its playlist. Pure and rune-free on
// purpose — `occurrences.check.ts` runs it under plain node (`node --experimental-strip-types`).
import type { Occurrence, RowRef, SongItem } from './api';

export type OccurrenceAction = 'remove' | 'move' | 'move_new';

/** A copy's identity in the dialog: its playlist and which copy of the song it is there. Stable
 *  across a re-read, unlike a position or a handle. */
export const occKey = (o: Occurrence): string => `${o.playlist_id}\u001f${o.nth}`;

export type OccurrencePlan = {
	/** The copies to touch, grouped by playlist, in the order the dialog lists them. */
	byPlaylist: Map<string, Occurrence[]>;
	/** How many copies the action really touches: what "Apply to N" says. */
	count: number;
	/** Ticked but left alone: a move's copies that already sit in the destination. */
	skipped: number;
};

/** What applying `action` to the ticked copies (`selected`, by `occKey`) would touch. A move to an
 *  existing playlist leaves out the copies already in it, and with no destination picked yet it
 *  touches nothing; a new playlist holds none of them. */
export function planOccurrences(
	occ: readonly Occurrence[],
	selected: ReadonlySet<string>,
	action: OccurrenceAction,
	destId: string | null
): OccurrencePlan {
	const byPlaylist = new Map<string, Occurrence[]>();
	let count = 0;
	let skipped = 0;
	for (const o of occ) {
		if (!selected.has(occKey(o))) continue;
		if (action === 'move') {
			if (!destId) continue;
			if (o.playlist_id === destId) {
				skipped++;
				continue;
			}
		}
		const list = byPlaylist.get(o.playlist_id);
		if (list) list.push(o);
		else byPlaylist.set(o.playlist_id, [o]);
		count++;
	}
	return { byPlaylist, count, skipped };
}

/** The rows of a fresh read (`fresh`, one playlist in order) that the picked copies of `videoId`
 *  are now. A copy is matched by its handle when that is still there, otherwise as the same nth
 *  copy of the song; one that can't be found (or has no handle: not yours to edit) is left out, as
 *  is one row claimed twice. Each row carries `before`, the handle of the next row that stays —
 *  where an undo puts it back (the rule of `everywhere.rs`'s `restore_rows`). In playlist order. */
export function resolveRows(fresh: readonly SongItem[], videoId: string, picked: readonly Occurrence[]): RowRef[] {
	const copies: number[] = [];
	fresh.forEach((r, i) => {
		if (r.video_id === videoId) copies.push(i);
	});
	const gone = new Set<number>();
	for (const o of picked) {
		let at = o.set_video_id ? copies.find((i) => fresh[i].set_video_id === o.set_video_id) : undefined;
		if (at === undefined) at = copies[o.nth];
		if (at === undefined || gone.has(at) || !fresh[at].set_video_id) continue;
		gone.add(at);
	}
	const out: RowRef[] = [];
	let next: string | null = null;
	for (let i = fresh.length - 1; i >= 0; i--) {
		const h = fresh[i].set_video_id;
		if (!h) continue;
		if (gone.has(i)) out.push({ song: fresh[i], before: next });
		else next = h;
	}
	return out.reverse();
}
