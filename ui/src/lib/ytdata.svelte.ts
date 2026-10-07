// The YouTube Data API's status (src-tauri/src/ytdata_status.rs), kept current for every component
// that shows a warning about it: read once on first use, then from `ytdata-status-changed`, which
// the backend sends after each sync that went through the API, whenever its marks change, and after
// the settings tab connects, disconnects or links a channel or imports a client secret.
import * as api from './api';
import type { PlaylistEngine, YtDataStatus } from './api';
import type { SettingsTab } from './player.svelte';

/** The playlist engine setting, as `playlist_engine` stores it. */
export type { PlaylistEngine };

export const ytdata = $state({
	/** Null until the first answer arrives. */
	status: null as YtDataStatus | null,
	engine: 'auto' as PlaylistEngine
});

let started = false;

/** Start listening (once) and read the status now. Safe to call from every component that cares. */
export function trackYtData() {
	if (started) return;
	started = true;
	void api.onYtDataStatus((s) => (ytdata.status = s)).catch(() => {});
	void refreshYtData();
}

/** Read the status and the engine setting again (after an action that may have changed them). */
export async function refreshYtData() {
	try {
		const [status, settings] = await Promise.all([api.ytdataStatus(), api.getSettings()]);
		ytdata.status = status;
		ytdata.engine = parseEngine(settings.playlist_engine);
	} catch {
		// Outside Tauri, or before the backend is up: no status, no warning.
	}
}

/** `playlist_engine` as stored, anything unknown reading as `auto` (what the backend does too). */
export function parseEngine(raw: string | undefined): PlaylistEngine {
	return raw === 'ytdata' || raw === 'innertube' ? raw : 'auto';
}

/** Whether to warn about the Data API where its functions are used. Not when it works, and not
 *  when nobody set it up while the engine is `auto` or `innertube` (that includes no keyring on
 *  Linux): nothing asked for it then, and InnerTube does the work. Once a channel is connected
 *  (anything past `not_configured`), or with the engine set to `ytdata`, a problem is worth
 *  saying. */
export function shouldWarn(status: YtDataStatus | null, engine: PlaylistEngine): boolean {
	if (!status || status.state === 'ok') return false;
	if (status.state === 'not_configured') return engine === 'ytdata';
	return true;
}

/** Whether a confirmation should show what a write costs on the Data API: when the API works, or
 *  when this write asks for it explicitly (it would wait for quota or a reconnection then). */
export function showsCost(status: YtDataStatus | null, choice: PlaylistEngine): boolean {
	return choice === 'ytdata' || (choice === 'auto' && status?.state === 'ok');
}

/** Settings ▸ YouTube Data API, where "Configure" leads. */
export const YTDATA_TAB: SettingsTab = 'ytdata';
