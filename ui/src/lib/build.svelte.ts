// A split or a merge being written (`playlist_tools/build.rs`). Shared state, so the tools dialog
// can be closed while it runs and the sidebar still shows how far it got.
import * as api from './api';
import type { Built } from './api';
import { loadLibrary, refreshLocalPlaylists, toast } from './player.svelte';
import { announceOp } from './playlistops.svelte';
import { t } from './i18n.svelte';

export const building = $state({ running: false, done: 0, total: 0, current: '' });

/** Run one build, word the outcome, and bring the library up to date with what it made. */
export async function runBuild(
	args: Parameters<typeof api.buildPlaylists>[0],
	message: (b: Built) => string
): Promise<Built | null> {
	if (building.running) {
		toast(t('build.busy'));
		return null;
	}
	building.running = true;
	building.done = 0;
	building.total = args.lists.reduce((n, l) => n + l.songs.length, 0);
	building.current = '';
	const off = await api.onPlaylistOpProgress((p) => {
		building.done = p.done;
		building.total = p.total;
		building.current = p.current;
	});
	try {
		const b = await api.buildPlaylists(args);
		if (b.created.some((p) => !api.isLocalPlaylist(p.id))) loadLibrary(true);
		if (b.created.some((p) => api.isLocalPlaylist(p.id))) refreshLocalPlaylists();
		if (b.error) toast.error(t('build.partial', { error: b.error, made: b.created.length }));
		else if (b.stopped) announceOp(b.op, t('build.stopped', { made: b.created.length }));
		else announceOp(b.op, message(b));
		return b;
	} catch (e) {
		toast.error(String(e));
		return null;
	} finally {
		off();
		building.running = false;
	}
}

export const stopBuild = () => void api.cancelPlaylistBuild().catch(() => {});
