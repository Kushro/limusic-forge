// The UI half of the playlist tools' undo (src-tauri/src/playlist_tools/journal.rs): every edit
// answers its journal entry, which turns into a toast with an Undo button, and the history dialog
// lists the last 20 with an Undo on each.
import * as api from './api';
import type { PlaylistOp } from './api';
import { loadLibrary, refreshLocalPlaylists, toast } from './player.svelte';
import { t } from './i18n.svelte';

/** Say what an edit did, with an Undo button while it can still be undone. */
export function announceOp(op: PlaylistOp | null, msg: string) {
	if (op?.undoable) toast.action(msg, t('undo.action'), () => void undoOp(op.id));
	else toast.success(msg);
}

/** Say what several edits did together (one action that wrote to several playlists), with one Undo
 *  that takes them back newest first: a later edit may stand on an earlier one. */
export function announceOps(ops: (PlaylistOp | null)[], msg: string) {
	const live = ops.filter((op): op is PlaylistOp => !!op?.undoable);
	if (!live.length) {
		toast.success(msg);
		return;
	}
	const ids = live.map((op) => op.id).reverse();
	toast.action(msg, t('undo.action'), () => void undoOps(ids));
}

/** Undo `ids` in that order, quietly, and say once how it went. Stops at the first refusal. */
async function undoOps(ids: number[]): Promise<boolean> {
	let rebuilt = false;
	try {
		for (const id of ids) {
			const op = await api.undoPlaylistOp(id);
			if (op.kind === 'split' || op.kind === 'merge' || op.kind === 'extract') rebuilt = true;
		}
		toast.success(t('undo.done'));
		return true;
	} catch (e) {
		toast.error(t('undo.failed', { error: String(e) }));
		return false;
	} finally {
		if (rebuilt) {
			loadLibrary(true);
			refreshLocalPlaylists();
		}
	}
}

/** Undo one edit. The backend announces the playlists it touched (`playlists-edited`), which is
 *  what makes an open page re-read them. Answers whether it went through. */
export async function undoOp(id: number): Promise<boolean> {
	try {
		const op = await api.undoPlaylistOp(id);
		toast.success(t('undo.done'));
		// Undoing a split, a merge or an extract deletes the playlists it made: the library has to
		// drop them.
		if (op.kind === 'split' || op.kind === 'merge' || op.kind === 'extract') {
			loadLibrary(true);
			refreshLocalPlaylists();
		}
		return true;
	} catch (e) {
		toast.error(t('undo.failed', { error: String(e) }));
		return false;
	}
}
