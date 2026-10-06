// The UI half of the playlist tools' undo (src-tauri/src/playlist_tools/journal.rs): every edit
// answers its journal entry, which turns into a toast with an Undo button, and the history dialog
// lists the last 20 with an Undo on each.
import * as api from './api';
import type { PlaylistOp } from './api';
import { toast } from './player.svelte';
import { t } from './i18n.svelte';

/** Say what an edit did, with an Undo button while it can still be undone. */
export function announceOp(op: PlaylistOp | null, msg: string) {
	if (op?.undoable) toast.action(msg, t('undo.action'), () => void undoOp(op.id));
	else toast.success(msg);
}

/** Undo one edit. The backend announces the playlists it touched (`playlists-edited`), which is
 *  what makes an open page re-read them. Answers whether it went through. */
export async function undoOp(id: number): Promise<boolean> {
	try {
		await api.undoPlaylistOp(id);
		toast.success(t('undo.done'));
		return true;
	} catch (e) {
		toast.error(t('undo.failed', { error: String(e) }));
		return false;
	}
}
