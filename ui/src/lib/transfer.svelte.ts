// Copying and moving tracks between playlists from the UI: a drop on a sidebar playlist
// (`Sidebar.svelte`, `DropConfirm.svelte`) and "Move to…" in the selection bar. The work is the
// backend's (`playlist_tools/transfer.rs`); this side picks the mode, words the toast and keeps the
// library's track counts in step.
import * as api from './api';
import type { BrowseItem } from './api';
import type { TrackRowsDrag } from './dnd';
import {
	bumpLibraryTrackCount,
	ownedByUser,
	prefs,
	toast,
	type DropDupes,
	type DropMode
} from './player.svelte';
import { announceOp } from './playlistops.svelte';
import { t } from './i18n.svelte';

/** A playlist rows can go into: yours to edit, and not the one they are being dragged out of. */
export function canDropOn(item: BrowseItem, from: string | null): boolean {
	if (item.kind !== 'playlist' || item.id === from) return false;
	if (item.id === api.LIKED_MUSIC_ID || item.id === api.ON_REPEAT_ID) return false;
	return api.isLocalPlaylist(item.id) || ownedByUser(item);
}

/** The mode a drop takes: Ctrl forces a copy, Shift a move, otherwise the saved preference. A list
 *  that isn't a playlist of yours can only be copied from. */
export function dropModeFor(drag: TrackRowsDrag, e: { ctrlKey: boolean; shiftKey: boolean; metaKey?: boolean }): DropMode {
	if (!drag.from) return 'copy';
	if (e.ctrlKey || e.metaKey) return 'copy';
	if (e.shiftKey) return 'move';
	return prefs.dropMode;
}

/** Keep the choice made in the drop popover ("Don't ask again"). */
export function rememberDrop(mode: DropMode, dupes: DropDupes) {
	prefs.dropMode = mode;
	prefs.dropDupes = dupes;
	api.setSetting('drop_mode', mode).catch(() => {});
	api.setSetting('drop_dupes', dupes).catch(() => {});
}

// One at a time: two drops racing would each read the target before the other wrote to it, and the
// second's duplicate check would be wrong.
let busy = false;

export async function transfer(
	drag: TrackRowsDrag,
	target: BrowseItem,
	mode: 'copy' | 'move',
	duplicates: DropDupes
): Promise<boolean> {
	if (busy) {
		toast(t('drop.busy'));
		return false;
	}
	if (!api.isLocalPlaylist(target.id) && drag.rows.every((r) => api.isLocalId(r.song.video_id))) {
		toast.error(t('selection.local_playlist'));
		return false;
	}
	busy = true;
	try {
		const res = await api.transferTracks({
			source: drag.from ? { id: drag.from, title: drag.fromTitle } : null,
			target: { id: target.id, title: target.title },
			rows: drag.rows,
			mode: drag.from ? mode : 'copy',
			duplicates
		});
		if (res.added) bumpLibraryTrackCount(target.id, res.added);
		if (res.removed && drag.from) bumpLibraryTrackCount(drag.from, -res.removed);
		const playlist = target.title;
		const moved = res.removed > 0;
		const main = !res.added && !res.removed
			? t('drop.nothing', { playlist })
			: moved
				? res.removed === 1
					? t('drop.moved_one', { playlist })
					: t('drop.moved', { count: res.removed, playlist })
				: res.added === 1
					? t('drop.copied_one', { playlist })
					: t('drop.copied', { count: res.added, playlist });
		const notes = [
			res.duplicates ? t('drop.dupes_note', { count: res.duplicates }) : '',
			res.refused ? t('drop.refused_note', { count: res.refused }) : ''
		].filter(Boolean);
		announceOp(res.op, [main, ...notes].join(' · '));
		return true;
	} catch (e) {
		toast.error(String(e));
		return false;
	} finally {
		busy = false;
	}
}
