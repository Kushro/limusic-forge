// The UI's only door to Rust. context/11 UI contract — commands in, events out. The UI never
// touches YouTube; everything here is a Tauri command or event payload.
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { t } from './i18n.svelte';

/** How the signed-in user rated a track (innertube `Rating`). The three states are mutually
 *  exclusive: liking a disliked track clears the dislike, and vice versa. */
export type Rating = 'like' | 'dislike' | 'indifferent';

/** One run of an artist line: its text, plus a channel id when that run links an artist. */
export interface ArtistRun {
	text: string;
	id?: string;
}

export interface SongItem {
	video_id: string;
	title: string;
	artists: string;
	/** Primary artist's channel browseId (`UC…`), when linked — makes the artist name navigable. */
	artist_id?: string;
	/** The artist line run by run — a collab links each name to its own page. Empty/absent when
	 * nothing is linked; render plain `artists` then. */
	artist_runs?: ArtistRun[];
	album?: string;
	/** The album's browseId (`MPRE…`), when linked — makes the album navigable. */
	album_id?: string;
	duration?: string;
	/** Play count as YouTube abbreviates it ("53M"). Album, artist and search rows. */
	play_count?: string;
	thumbnail?: string;
	/** Item id within a playlist — present only on playlist tracks; needed to remove them. */
	set_video_id?: string;
	/** Collaborative playlists only: who added this track, and their avatar. */
	added_by?: string;
	added_by_avatar?: string;
	/** The signed-in user's rating (absent when the response didn't say — same as 'indifferent'). */
	rating?: Rating;
	/** "Add to library" off the row's own menu, with a token for each direction. Absent on rows
	 *  YouTube sent no menu for, and on the ones built here (local files, On Repeat, a Listen
	 *  Together guest's queue) — the menu hides the action rather than offering a dead one.
	 *  Library ▸ Songs is not Liked Music: this is a feedback write, not a rating. */
	library?: { in_library: boolean; add_token?: string; remove_token?: string };
	/** Listen Together: name of the guest who added this queue item (session adds only). */
	queued_by?: string;
	/** Queued to play next ("Play next", or a guest's session add) — the "Next in queue" block. */
	queued?: boolean;
	/** Appended by "Add to queue" — its own block at the tail of the queue. */
	queued_end?: boolean;
	/** The album/playlist either block was added from, for its heading in the queue panel. */
	queued_from?: string;
	/** Appended by autoplay radio continuation — drives the queue's "Autoplay" divider + badge. */
	autoplay?: boolean;
	/** YouTube flags the track explicit. Browse/search rows only: `/next` carries no badge, so a
	 *  radio- or autoplay-appended track arrives without it. */
	explicit?: boolean;
	/** This row links a music video rather than the audio track. */
	is_video?: boolean;
	/** One of the user's own YouTube Music uploads. Set by Rust and passed straight back on play:
	 *  only an authenticated client can stream one, and the row is where that is known. */
	is_upload?: boolean;
	/** A playlist row YouTube greys out: taken down, made private, or blocked where you are. */
	unavailable?: boolean;
}

export interface NowPlaying {
	videoId: string;
	title: string;
	artists: string;
	artistId?: string;
	/** The artist line run by run — links each artist of a collab separately. */
	artistRuns?: ArtistRun[];
	thumbnail?: string;
	duration?: string;
	album?: string | null;
	streamClient: string;
	/** The user's rating of the track (null if unknown). */
	rating?: Rating | null;
	/** YouTube's `musicVideoType` says this is a video upload, not the generated audio track.
	 *  Gates the player view's music-video mode. */
	isVideo?: boolean;
}

export type RepeatMode = 'off' | 'all' | 'one';

export interface QueueState {
	items: SongItem[];
	currentIndex: number;
	shuffle?: boolean;
	repeat?: RepeatMode;
	/** What seeded the queue (playlist/album title, "<song> Radio") — the "Next from" header. */
	sourceName?: string | null;
	/** The playlist the queue was started from, when it was one. What "Remove from this playlist"
	 *  in the player's track menu writes to; absent for radios, single songs and guest queues. */
	sourceId?: string | null;
	/** Title of the track a click replaced this queue mid-play, for the panel's "Back to …" line.
	 *  Set only while `backToPrevious` would actually do something: at the head of the queue, with
	 *  a kept one behind it. */
	prevTrack?: string | null;
}

export interface Account {
	signedIn: boolean;
	name?: string | null;
	handle?: string | null;
	email?: string | null;
	thumbnail?: string | null;
	channelId?: string | null;
	canSwitch?: boolean;
	/** The cookie authenticated, but a multi-channel login is not complete until one is chosen. */
	selectionRequired?: boolean;
}

export interface AccountIdentity {
	/** Opaque, process-local selector. Raw delegated/data-sync ids stay in Rust. */
	selectionKey: string;
	name: string;
	handle?: string | null;
	email?: string | null;
	thumbnail?: string | null;
	channelId?: string | null;
	selected: boolean;
}

/** One saved Google account (multi-account). Display fields only — cookies stay in Rust. */
export interface SavedAccount {
	/** Opaque, process-local selector. */
	id: string;
	name?: string | null;
	handle?: string | null;
	email?: string | null;
	thumbnail?: string | null;
	/** The account whose session is currently driving requests. */
	active: boolean;
}

export interface BrowseItem {
	kind: 'song' | 'playlist' | 'album' | 'artist';
	/** videoId (song) or browseId (playlist/album/artist). */
	id: string;
	title: string;
	subtitle?: string;
	thumbnail?: string;
	/** "3:47" — song items from a list-style shelf only (card shelves don't carry one). */
	duration?: string;
	/** Song cards only: the track's album (`MPRE…`), what puts "Go to album" in its menus. */
	albumId?: string;
	/** Song cards only: the artist line run by run, so a card that gets played keeps its links. */
	artistRuns?: ArtistRun[];
	/** Play count as YouTube abbreviates it ("2.5B") — search song rows only. */
	playCount?: string;
	/** YouTube flags this track/album explicit. */
	explicit?: boolean;
	/** Song cards only: one of the user's own uploads. Carried into the SongItem `asSong` builds,
	 *  because that flag is what picks the login-only client chain when it plays. */
	isUpload?: boolean;
}

export interface HomeSection {
	title: string;
	items: BrowseItem[];
	moreBrowseId?: string;
	moreParams?: string;
}
/** A mood/genre filter chip above the home feed; `params` re-fetches home filtered to it. */
export interface HomeChip {
	title: string;
	params: string;
}
export interface HomePage {
	chips: HomeChip[];
	sections: HomeSection[];
	continuation?: string;
}

/**
 * The On Repeat auto-playlist's synthetic browseId (mirrors `ON_REPEAT_ID` in state.rs). It routes
 * like any other playlist; the only thing the UI does differently is draw an icon cover, because
 * a playlist built from local play counts has no artwork of its own.
 */
export const ON_REPEAT_ID = 'LIMUSIC_ON_REPEAT';

/**
 * Liked Music's browseId. YouTube edits this one through the rating endpoint, not `edit_playlist`,
 * so it is never an add/remove/rename target: liking the song is the edit.
 */
export const LIKED_MUSIC_ID = 'VLLM';

/**
 * YouTube Music's own Library ▸ Songs, despite the name: the songs saved to the account's library.
 * It browses like a playlist (no header, no sort menu), so `getPlaylist` reads it and the Library
 * page's Songs tab pages through it with `getPlaylistMore`.
 */
export const LIBRARY_SONGS_ID = 'FEmusic_liked_videos';

/**
 * The tracks the signed-in user uploaded to YouTube Music themselves. Browses like the songs grid
 * above, so the same tab component reads it; the rows come back with `is_upload` set, which is what
 * sends them down the login-only fallback chain when they play (issue #71).
 */
export const LIBRARY_UPLOADS_ID = 'FEmusic_library_privately_owned_tracks';

/**
 * Local music (Rust `local.rs`). A file on disk is a song whose `video_id` is `LOCAL:<path>`, and
 * an album of them is a browseId `LOCALALBUM:<key>` — so local items ride every existing surface
 * (cards, queue, Shortcuts, the album page) and play with no network.
 */
export const LOCAL_SONG_PREFIX = 'LOCAL:';
export const LOCAL_ALBUM_PREFIX = 'LOCALALBUM:';
/** An artist on this disk. Renders through the album route: same page, no YouTube channel. */
export const LOCAL_ARTIST_PREFIX = 'LOCALARTIST:';
/**
 * A playlist kept on this machine, no account needed (#251; mirrors `LOCAL_PLAYLIST_PREFIX` in
 * state.rs). The playlist commands answer it from SQLite, so it rides the same route and the same
 * calls as a YouTube playlist, and can hold local files as well as YouTube tracks.
 */
export const LOCAL_PLAYLIST_PREFIX = 'LOCALPLAYLIST:';
export const isLocalPlaylist = (id: string | undefined | null): boolean =>
	!!id && id.startsWith(LOCAL_PLAYLIST_PREFIX);
/** Anything with no YouTube item behind it: nothing to share, no radio, no account to save it to. */
export const isLocalId = (id: string | undefined | null): boolean =>
	!!id &&
	(id.startsWith(LOCAL_SONG_PREFIX) ||
		id.startsWith(LOCAL_ALBUM_PREFIX) ||
		id.startsWith(LOCAL_ARTIST_PREFIX) ||
		id.startsWith(LOCAL_PLAYLIST_PREFIX));

export interface LocalLibrary {
	/** Watched folders, as absolute paths. */
	folders: string[];
	albums: BrowseItem[];
	artists: BrowseItem[];
	songs: SongItem[];
	/** Song/album/artist ids that were in the library but are gone from disk since the last scan. */
	removed: string[];
}

/** The orders YouTube itself can put a playlist in — everything in `SortKey` but our own `plays`. */
export type ServerSort = 'default' | 'newest' | 'oldest' | 'title' | 'artist' | 'album' | 'top';

export interface SortMenu {
	/** The order YouTube has this list in right now, when it is one we have a name for. */
	selected?: ServerSort;
	/**
	 * The choice is a write, so storing it makes YouTube Music and every other client follow.
	 * Playlists you own only: elsewhere the menu is view-only (Liked Music remembers the last order
	 * asked for anyway, someone else's playlist does not).
	 */
	editable: boolean;
}

/** One day bucket of the play history: YouTube's own heading plus that day's rows. */
export interface HistoryGroup {
	title: string;
	items: SongItem[];
}

export interface PlaylistPage {
	title?: string;
	subtitle?: string;
	thumbnail?: string;
	/** The playlist's own blurb, which the edit dialog prefills its description with. */
	description?: string;
	/** `PUBLIC` / `PRIVATE` / `UNLISTED`. Only playlists you own report it. */
	privacy?: string;
	/** Custom artwork picked on this machine; falls back to `thumbnail` when unset. */
	cover?: string;
	items: SongItem[];
	continuation?: string;
	/** True only when the signed-in user owns this playlist (rename/delete allowed). */
	owned: boolean;
	/** Collaboration is on: others can add to it, and each person may remove only what they added. */
	collaborative: boolean;
	/** Absent on lists YouTube will not reorder: albums, its own radio mixes, On Repeat. */
	sortMenu?: SortMenu;
}
export interface PlaylistContinuation {
	items: SongItem[];
	continuation?: string;
}

export interface ArtistCarousel {
	title: string;
	items: BrowseItem[];
	moreBrowseId?: string;
	moreParams?: string;
}
export interface SearchResults {
	top: BrowseItem[];
	songs: BrowseItem[];
	albums: BrowseItem[];
	artists: BrowseItem[];
	playlists: BrowseItem[];
}

/** Typeahead under a search field: query completions, then a few matching rows. */
export interface SearchSuggestions {
	queries: { text: string; /** One of the account's own past searches. */ history: boolean }[];
	items: BrowseItem[];
}

export interface AlbumPage {
	title?: string;
	artist?: string;
	artistId?: string;
	/** The artist line run by run — links each artist of a collaborative album separately. */
	artistRuns?: ArtistRun[];
	artistThumbnail?: string;
	subtitle?: string;
	secondSubtitle?: string;
	description?: string;
	thumbnail?: string;
	items: SongItem[];
	continuation?: string;
	/** The album itself is flagged explicit (the header wears the badge, not just some tracks). */
	explicit?: boolean;
	/** The album's audio playlist id (`OLAK5uy_…`) — autoplay's radio seed, and the save target. */
	playlistId?: string;
	/** Already saved to the signed-in user's library. */
	inLibrary: boolean;
	/** Card shelves under the tracks: other versions, more from the artist, related releases. */
	sections?: ArtistCarousel[];
}

export interface ArtistPage {
	name?: string;
	thumbnail?: string;
	description?: string;
	subscribers?: string;
	monthlyListeners?: string;
	channelId: string;
	subscribed: boolean;
	topSongs: SongItem[];
	/** `VL…` playlist of all the artist's top songs, behind the shelf's "See all". */
	topSongsId?: string;
	sections: ArtistCarousel[];
}

// --- commands (context/11) -----------------------------------------------------------------
// `recordHistory` is true only for a query the user submitted: a signed-in search is written to the
// account's YouTube search history, so a typeahead preview must stay anonymous (#203).
export const search = (query: string, recordHistory = false) =>
	invoke<SongItem[]>('search', { query, recordHistory });
/** Video uploads only: covers, live sets and remixes with no official release. Empty when the
 *  "hide music videos" setting is on. */
export const searchVideos = (query: string) => invoke<SongItem[]>('search_videos', { query });
/** Unfiltered search → categorized sections. */
export const searchAll = (query: string, recordHistory = false) =>
	invoke<SearchResults>('search_all', { query, recordHistory });
/** The typeahead. Signed in, yet never written to search history: it's the request YTM's own
 *  search box sends on every keystroke. */
export const searchSuggestions = (query: string) =>
	invoke<SearchSuggestions>('search_suggestions', { query });
/** Filtered "Show more" card search for one category (albums / artists / playlists). */
export const searchCards = (query: string, category: 'albums' | 'artists' | 'playlists') =>
	invoke<BrowseItem[]>('search_cards', { query, category });
export const play = (item: SongItem) => invoke<void>('play', { item });
export const playIndex = (index: number) => invoke<void>('play_index', { index });
/** Remove an upcoming track from the queue (host/local only — guests are add-only). */
export const removeFromQueue = (index: number) => invoke<void>('remove_from_queue', { index });
/** Drag-to-reorder: move the upcoming queue item at `from` to index `to` (both past the playing
 * track — the history and the playing row don't move). */
export const moveInQueue = (from: number, to: number) =>
	invoke<void>('move_in_queue', { from, to });
/**
 * "Play next": insert tracks right behind the playing one, behind any earlier "Play next" adds.
 * `from` is the album/playlist they came from.
 */
export const playNext = (items: SongItem[], from?: string) =>
	invoke<void>('play_next', { items, from });
/**
 * "Add to queue": the tracks go at the tail of the queue, behind the rest of the playing album or
 * playlist and anything added before, ahead of autoplay (#369). On a radio they go ahead of the
 * generated tracks instead. `continuation` is the source page's next-page token: the backend walks
 * the rest of a long playlist into the queue in the background.
 */
export const addToQueue = (items: SongItem[], from?: string, continuation?: string) =>
	invoke<void>('add_to_queue', { items, from, continuation });
/** Clear every upcoming track added by hand, with Play next or Add to queue. */
export const clearQueued = () => invoke<void>('clear_queued');
export const nextTrack = () => invoke<void>('next_track');
export const prevTrack = () => invoke<void>('prev_track');
/** Put back the queue a click replaced, at the track and position it was left at. Previous does
 *  this too, but only from the top of a track. */
export const backToPrevious = () => invoke<void>('back_to_previous');
export const toggleShuffle = () => invoke<void>('toggle_shuffle');
export const setRepeat = (mode: RepeatMode) => invoke<void>('set_repeat', { mode });
export const togglePause = () => invoke<void>('toggle_pause');
export const seek = (position: number) => invoke<void>('seek', { position });
export const setVolume = (volume: number) => invoke<void>('set_volume', { volume });
/** Tempo (0.25–2.0) + pitch (−12..=12 semitones). Not persisted: resets on restart. */
export const setPlaybackParams = (speed: number, semitones: number) =>
	invoke<void>('set_playback_params', { speed, semitones });
export const getQueue = () => invoke<QueueState>('get_queue');
/** A `limusicvideo://` URL for the track's music video, or null when there isn't one. `maxHeight`
 *  caps the picture at what the box on screen can actually show. The bytes are proxied through
 *  Rust; the webview never sees a googlevideo URL. */
export const videoStream = (videoId: string, maxHeight: number) =>
	invoke<string | null>('video_stream', { videoId, maxHeight });

/** Drop the backend's memory of this track's video URL, after the element failed to load it. */
export const forgetVideoStream = (videoId: string) =>
	invoke<void>('forget_video_stream', { videoId });

/** Linux and Windows: where the page's hole for the music video is (`[x, y, w, h]`, CSS pixels,
 *  relative to the viewport), or null when there is none. mpv draws the picture there, under the
 *  webview. Resolves whether the picture is up; `false` for a rect means it never will be (no
 *  surface), so fall back to the `<video>` element. `dpr` carries the page zoom to Windows. */
export const nativeVideoRect = (rect: [number, number, number, number] | null) =>
	invoke<boolean>('native_video_rect', { rect, dpr: devicePixelRatio });

/** Linux and Windows: the newest small frame of mpv's picture other than `after`, for the ambient
 *  light, as `[seq, w, h]` little-endian u32s and then RGBA rows bottom-up. Empty when there is no
 *  new one (Linux waits a quarter second for it). Asking is also what keeps Rust grabbing them
 *  (nativevideo.rs; on Windows each ask is one grab, nativevideo_windows.rs).
 *  An ArrayBuffer, except once Tauri has fallen back from its custom protocol to postMessage (it
 *  does for the rest of the page's life after any IPC fetch fails): raw bytes then arrive as a
 *  plain array of numbers. */
export const ambientFrame = (after: number) =>
	invoke<ArrayBuffer | number[]>('ambient_frame', { after });

/** What the event stream already reported, for a webview that started after it did. */
export interface PlaybackSnapshot {
	now: NowPlaying | null;
	paused: boolean;
	position: number;
	duration: number;
	/** The level restored from last run (or the one another window already set). */
	volume: number;
}
export const getPlayback = () => invoke<PlaybackSnapshot>('get_playback');

// --- settings (context/11) -----------------------------------------------------------------
/** Stored settings plus a few read-only, derived ones (`native_chrome`, `native_video`,
 *  `discord_available`). Every value is a string, booleans as `'true'`/`'false'`. */
export type Settings = Record<string, string> & {
	/** Whether this build was compiled with a Discord application id (rich presence can work). */
	discord_available?: 'true' | 'false';
};
export const getSettings = () => invoke<Settings>('get_settings');
export const setSetting = (key: string, value: string) =>
	invoke<void>('set_setting', { key, value });
/** Why the database's schema upgrade failed at startup, or `null` if it did not. */
export const dbMigrationError = () => invoke<string | null>('db_migration_error');
/** Streamable client keys for the "disabled clients" setting. */
export const getStreamClients = () => invoke<string[]>('get_stream_clients');
/** Wipe both cache tiers (URL cache + mpv on-disk audio cache). */
export const clearCaches = () => invoke<void>('clear_caches');
/** Set the app icon to a PNG the user picked, or restore the bundled one with `null` (#173). */
export const setAppIcon = (path: string | null) => invoke<void>('set_app_icon', { path });
/** Path to the custom app icon, granted to the asset protocol. `null` when the bundled one is in use. */
export const appIconPath = () => invoke<string | null>('app_icon_path');

/** Grant the webview a URL for one font file the user picked, so `@font-face` can load it. */
export const allowFontFile = (path: string) => invoke<void>('allow_font_file', { path });

// --- global hotkeys -------------------------------------------------------------------------
export interface HotkeysConfig {
	enabled: boolean;
	bindings: Record<string, string>;
}

export interface HotkeyRegisterResult {
	success: boolean;
	config: HotkeysConfig;
	errors: Record<string, string>;
}

export const getGlobalHotkeys = () => invoke<HotkeysConfig>('get_global_hotkeys');
export const globalHotkeysOnWayland = () => invoke<boolean>('global_hotkeys_on_wayland');
export const setGlobalHotkeys = (config: HotkeysConfig) =>
	invoke<HotkeyRegisterResult>('set_global_hotkeys', { config });
export const resetGlobalHotkeys = () => invoke<HotkeyRegisterResult>('reset_global_hotkeys');

/** One published release: the GitHub release description, verbatim markdown. */
export interface ReleaseNote {
	version: string;
	/** `YYYY-MM-DD` */
	date: string;
	body: string;
}
/** Changelog for Settings > About, from the GitHub releases API (cached in Rust per run). */
export const releaseNotes = () => invoke<ReleaseNote[]>('release_notes');
/** False on Linux builds that aren't the AppImage (.rpm, the AUR package): they update through the
 *  package manager, so the UI offers a download link instead of an install button. */
export const canSelfUpdate = () => invoke<boolean>('can_self_update');
export interface InstallInfo {
	/** Running from a folder with a `data` directory next to the exe (paths.rs). */
	portable: boolean;
	/** Where the database, log and caches live. */
	data_dir: string;
}
/** Portable or installed, for Settings > About and the autostart toggle. */
export const installInfo = () => invoke<InstallInfo>('install_info');

// --- import & migrate (migrate_upstream.rs) --------------------------------------------------
/** What there is to import on this machine. Detection only. */
export interface ImportSources {
	/** Upstream LiMusic's data folder: where, how big, and whether LiMusic is open right now. */
	upstream: { path: string; bytes: number; running: boolean } | null;
	/** PlaylistForge's database, when there is one. */
	playlistforge: string | null;
}
export const importSources = () => invoke<ImportSources>('import_sources');
/** Leave the migration marker and restart: the copy runs on the next launch, before anything
 *  opens the database. Rejects with `upstream_running` while LiMusic is open. */
export const migrateUpstreamRequest = (includeWebview: boolean) =>
	invoke<void>('migrate_upstream_request', { includeWebview });
export interface MigrateReport {
	/** `retry`: LiMusic was open, the next launch tries again. `expired`: the request was more than
	 *  ten minutes old, so nothing was done; the marker waits for "retry now" or "cancel". */
	status: 'done' | 'retry' | 'expired' | 'error';
	error: string | null;
	files: number;
	bytes: number;
	/** Where this app's previous data was moved to, if it had any. */
	aside: string | null;
	webview: boolean;
	/** Whether LiMusic starts at login (only reported after a successful copy). */
	upstream_autostart: boolean | null;
}
/** What the last migration did; read once, then gone. */
export const migrateUpstreamResult = () => invoke<MigrateReport | null>('migrate_upstream_result');
/** A migration asked for and not carried out yet. Only valid for ten minutes after
 *  `requested_at`; past that (`expired`) the next launch no longer acts on it on its own. */
export interface MigratePending {
	/** Unix seconds. */
	requested_at: number;
	expired: boolean;
	/** What the last launch made of it: `retry` (LiMusic open, a file held) or `expired`. */
	last_status: string | null;
	include_webview: boolean;
}
export const migrateUpstreamPending = () => invoke<MigratePending | null>('migrate_upstream_pending');
/** Drop the pending migration marker (only that file). */
export const migrateUpstreamCancel = () => invoke<void>('migrate_upstream_cancel');
/** The updater plugin's `check()` against the beta channel's manifest, as the metadata the
 *  plugin's `Update` class is built from. `null` when this build is what the channel offers. */
export const checkBetaUpdate = () =>
	invoke<ConstructorParameters<typeof import('@tauri-apps/plugin-updater').Update>[0] | null>(
		'check_beta_update'
	);
/** Open an http(s) link in the real browser, never in the webview itself. */
export const openExternal = (url: string) => invoke<void>('open_external', { url });

/** Environment + the redacted tail of `limusic.log`, for pasting into a bug report. */
export const diagnostics = () => invoke<string>('diagnostics');
/** Just the environment block, for prefilling the GitHub bug form. */
export const diagnosticsSummary = () => invoke<string>('diagnostics_summary');
/** The same text, written to a path the user picked in a save dialog. */
export const saveDiagnostics = (path: string) => invoke<void>('save_diagnostics', { path });

// --- auth (context/15) ---------------------------------------------------------------------
export const getAccount = () => invoke<Account>('get_account');
export const getAccountIdentities = () =>
	invoke<AccountIdentity[]>('get_account_identities');
export const switchAccount = (selectionKey: string) =>
	invoke<Account>('switch_account', { selectionKey });
export const signOut = () => invoke<void>('sign_out');
/**
 * Open the in-app Google sign-in webview (context/15 Path A). Result arrives via onAuthChanged.
 * With `addAccount`, Google's AddSession screen is used so a second account can be added even
 * while the webview already holds a Google session.
 */
export const loginWebview = (addAccount = false) => invoke<void>('login_webview', { addAccount });
/** Saved Google accounts for the account menu (display fields only). */
export const getGoogleAccounts = () => invoke<SavedAccount[]>('get_google_accounts');
/** Activate a saved account without a Google re-login. Fails if its stored session expired. */
export const switchGoogleAccount = (id: string) =>
	invoke<Account>('switch_google_account', { id });
/** Delete a saved account; removing the active one signs out. */
export const removeGoogleAccount = (id: string) => invoke<void>('remove_google_account', { id });

// --- mini player (Rust mini.rs) ---------------------------------------------------------------
/** Hide the app to the tray and open the floating widget (a second window running this same SPA). */
export const openMini = () => invoke<void>('open_mini');
/** Close the widget and bring the app back. */
export const closeMini = () => invoke<void>('close_mini');
/** Shrink the widget to its compact size, or back (#301). Remembered for the next open. */
export const setMiniCompact = (compact: boolean) => invoke<void>('set_mini_compact', { compact });

// --- browse / library (context/08) ---------------------------------------------------------
/** `params` is a `HomeChip.params` token — omit for the unfiltered feed. */
export const getHome = (params?: string) => invoke<HomePage>('get_home', { params });
export const getHomeMore = (token: string) => invoke<HomePage>('get_home_more', { token });
/**
 * On Repeat is the app's own playlist (Rust builds it from this machine's play counts), so its
 * title and subtitle are our English rather than YouTube's, and Rust cannot translate them: the
 * UI language lives in the webview's localStorage and never reaches it. Relabelled here, on the
 * way in, because every surface that draws the tile reads it from one of these two calls.
 */
const relabelOnRepeat = (item: BrowseItem): BrowseItem =>
	item.id !== ON_REPEAT_ID
		? item
		: {
				...item,
				title: t('library.on_repeat'),
				// The count is Rust's leading number ("20 songs"); left alone if it ever isn't.
				subtitle: Number.isNaN(parseInt(item.subtitle ?? '', 10))
					? item.subtitle
					: t('library.songs_count', { count: parseInt(item.subtitle!, 10) })
			};

/** Same reason as On Repeat: Rust's "12 songs" on a playlist kept on this machine. */
const relabelLocal = (item: BrowseItem): BrowseItem => {
	if (!isLocalPlaylist(item.id)) return relabelOnRepeat(item);
	const count = parseInt(item.subtitle ?? '', 10);
	if (Number.isNaN(count)) return item;
	return {
		...item,
		subtitle:
			count === 1
				? t('library.local_playlist_subtitle_one')
				: t('library.local_playlist_subtitle', { count })
	};
};

/** Every playlist in the library: On Repeat, the ones on this machine, then the account's. */
export const getLibrary = () =>
	invoke<BrowseItem[]>('get_library').then((items) => items.map(relabelLocal));
/** Just the playlists on this machine. SQLite only, so it answers offline and signed out. */
export const getLocalPlaylists = () =>
	invoke<BrowseItem[]>('local_playlists').then((items) => items.map(relabelLocal));
export const getLibraryAlbums = () => invoke<BrowseItem[]>('get_library_albums');
export const getLibraryArtists = () => invoke<BrowseItem[]>('get_library_artists');
/** Library ▸ Artists ▸ Subscriptions: the channels the account subscribes to. */
export const getLibrarySubscriptions = () => invoke<BrowseItem[]>('get_library_subscriptions');
export const getUploadAlbums = () => invoke<BrowseItem[]>('get_upload_albums');
/**
 * The account's YouTube Music play history, in YouTube's own day buckets (Today, Yesterday, …).
 * Empty when signed out.
 */
export const getHistory = () => invoke<HistoryGroup[]>('get_history');
/**
 * `sort` asks YouTube to order the tracks; omit it to get whatever order the account already has
 * the list in, which is the one a fresh visit wants (it is what YouTube Music would show).
 */
export const getPlaylist = (id: string, sort?: ServerSort, desc?: boolean) =>
	invoke<PlaylistPage>('get_playlist', { id, sort, desc }).then((page) =>
		id !== ON_REPEAT_ID
			? page
			: {
					...page,
					title: t('library.on_repeat'),
					subtitle: t('library.on_repeat_subtitle', { count: page.items.length })
				}
	);
/**
 * Store a sort order on a playlist, so YouTube Music and every other client show it the same way.
 * Only for a list whose `sortMenu.editable` is true.
 */
export const setPlaylistSort = (playlistId: string, sort: ServerSort) =>
	invoke<void>('set_playlist_sort', { playlistId, sort });
export const getPlaylistMore = (token: string) =>
	invoke<PlaylistContinuation>('get_playlist_more', { token });
/**
 * videoId → the ids of the playlists you own that hold it. Read straight from local SQLite, so it
 * answers instantly and is empty until `syncPlaylistIndex` has filled it in at least once.
 */
export const playlistIndex = () => invoke<Record<string, string[]>>('playlist_index');
/** Who started a sync, as `monitor_runs` files it. */
export type SyncTrigger = 'manual_ui' | 'scheduler' | 'headless';
/**
 * Re-walk your own playlists and answer with the rebuilt map. Skips the crawl while the stored one
 * is still inside `monitor_interval_hours`, so calling this on every launch is cheap; `force` crawls
 * anyway. Rejects with `busy` while another sync runs (its `onPlaylistIndexSynced` will follow).
 */
export const syncPlaylistIndex = (opts: { force?: boolean; trigger?: SyncTrigger } = {}) =>
	invoke<Record<string, string[]>>('sync_playlist_index', {
		force: opts.force ?? null,
		trigger: opts.trigger ?? null
	});
/** One sync run, summed over its playlists. */
export type SyncSummary = {
	at: number;
	trigger: SyncTrigger;
	/** Playlists read (yours; the ones merely saved are skipped). */
	playlists: number;
	/** Read to the end. */
	complete: number;
	/** Could not be read at all. */
	failed: number;
	added: number;
	removed: number;
	moved: number;
	unavailable: number;
	restored: number;
	alerts_new: number;
	/** Data API units the run spent; 0 when it read through InnerTube. */
	units_spent?: number;
};
/** Sync one playlist now, interval or not. Rejects with `busy` while another sync runs, and with
 *  `unreadable` when YouTube would not hand the playlist over. */
export const syncPlaylist = (playlistId: string) =>
	invoke<SyncSummary>('sync_playlist', { playlistId });
/** The last full sync's summary, null before the first. */
export const lastSyncSummary = () => invoke<SyncSummary | null>('last_sync_summary');
/** One playlist's last complete sync: when (epoch seconds), how many items it held, and what that
 *  sync found changed. */
export type PlaylistSyncInfo = {
	synced_at: number;
	item_count: number;
	added: number;
	removed: number;
	moved: number;
	/** The privacy the last Data API sync read; absent until one did (InnerTube does not say). */
	privacy?: PlaylistPrivacy;
};
export type PlaylistPrivacy = 'public' | 'unlisted' | 'private';
/** Playlist id → its last complete sync; playlists never synced are absent. SQLite only. */
export const playlistSyncInfo = () =>
	invoke<Record<string, PlaylistSyncInfo>>('playlist_sync_info');
/** `videoId` → the earliest date (epoch seconds) it was added to one of your playlists, as the Data
 *  API reported it. Tracks only InnerTube has read are absent. SQLite only. */
export const playlistAddedDates = () => invoke<Record<string, number>>('playlist_added_dates');
/** Whether the YouTube Data API can be used now (src-tauri/src/ytdata_status.rs). Precedence:
 *  not_configured > api_disabled > needs_auth > quota_exhausted > ok. States and counters only. */
export type YtDataState =
	| 'not_configured'
	| 'needs_auth'
	| 'quota_exhausted'
	| 'api_disabled'
	| 'ok';
export type YtDataReason =
	| 'no_client_secret'
	| 'no_account_for_active'
	| 'invalid_grant'
	| 'no_refresh_token'
	| 'keyring_unavailable'
	| 'secret_undecryptable';
export type YtDataStatus = {
	state: YtDataState;
	reason?: YtDataReason;
	/** The channel id the Data API acts as, and its title. */
	account?: string;
	account_title?: string;
	spent_today: number;
	daily_units: number;
	/** The next quota reset (midnight Pacific), RFC 3339. */
	next_reset: string;
};
export const ytdataStatus = () => invoke<YtDataStatus>('ytdata_status');
export const onYtDataStatus = (cb: (s: YtDataStatus) => void): Promise<UnlistenFn> =>
	listen<YtDataStatus>('ytdata-status-changed', (e) => cb(e.payload));

// --- Settings ▸ YouTube Data API ------------------------------------------------------------
// Only the masked client id, channel metadata and states come back: never a token, never the
// client secret (commands.rs).
/** The imported `client_secret.json`, as much of it as the UI may see. */
export type ClientSecretInfo = { masked_client_id: string };
/** Validate the picked `client_secret.json` and copy it into the data folder. */
export const ytdataImportClientSecret = (path: string) =>
	invoke<ClientSecretInfo>('ytdata_import_client_secret', { path });
/** The imported client secret, or null before one is. */
export const ytdataClientSecretInfo = () =>
	invoke<ClientSecretInfo | null>('ytdata_client_secret_info');
/** Start connecting a channel: Google's consent page opens in the system browser. The end arrives
 *  as `onYtDataConnectFinished` with the same `attempt`. */
export const ytdataConnectStart = () =>
	invoke<{ authorize_url: string; attempt: number }>('ytdata_connect_start');
export const ytdataConnectCancel = () => invoke<void>('ytdata_connect_cancel');
export type YtDataConnectFinished = {
	attempt: number;
	ok: boolean;
	code?: 'cancelled' | 'timed_out' | 'failed';
	error?: string;
	channel_id?: string;
	title?: string;
};
export const onYtDataConnectFinished = (cb: (r: YtDataConnectFinished) => void): Promise<UnlistenFn> =>
	listen<YtDataConnectFinished>('ytdata-connect-finished', (e) => cb(e.payload));
/** A channel connected through the Data API. `linked_account` is the cookie account (the `id` of
 *  `getGoogleAccounts`) it acts for. */
export type YtDataAccount = {
	channel_id: string;
	title: string;
	thumb: string | null;
	status: 'connected' | 'reauth_required';
	linked_account: string | null;
	added_at: number;
};
export const ytdataAccounts = () => invoke<YtDataAccount[]>('ytdata_accounts');
/** Revoke and forget a channel's sign-in. Its jobs stay, without an account. */
export const ytdataDisconnect = (channelId: string) =>
	invoke<void>('ytdata_disconnect', { channelId });
/** Pair a channel with a cookie account, or unpair it with null. */
export const ytdataLinkAccount = (channelId: string, account: string | null) =>
	invoke<void>('ytdata_link_account', { channelId, account });
/** The budget settings as they read (defaults filled in) and today's partition of the quota. The
 *  settings themselves are written with `setSetting` (`budget.*`), which validates them. */
export type BudgetInfo = {
	daily_units: number;
	safety_margin_percent: number;
	safety_margin_units: number;
	backup_reserve_units: number;
	backup_reserve_remaining_today: number;
	opportunistic_mode: boolean;
	backup_runs_per_day: number;
	spent_today: number;
	jobs_spent_today: number;
	backup_spent_today: number;
	available_for_jobs_now: number;
	next_reset: string;
};
export const budgetGet = () => invoke<BudgetInfo>('budget_get');

/** `playlist_engine`, or one operation's choice of engine. */
export type PlaylistEngine = 'auto' | 'ytdata' | 'innertube';
/** What a copy, move or removal would cost on the Data API, what jobs may still spend today, and
 *  the engine it would run on (null: it touches a playlist on this computer, nothing to spend). */
export type OpEstimate = {
	units: number;
	available: number;
	engine: 'ytdata' | 'innertube' | null;
	state: YtDataState;
};
export const estimateOp = (
	kind: 'copy' | 'move' | 'remove',
	params: { rows: number; playlists: string[]; playlist_len?: number; engine?: PlaylistEngine | null }
) => invoke<OpEstimate>('estimate_op', { kind, params });

/** The engine the next playlist writes (`transferTracks`, `removeTracks`) ask for when their caller
 *  names none: set by a confirmation that offers the choice (the drop popover, the tools dialog)
 *  around the writes it starts. Null leaves it to `playlist_engine`. */
let engineChoice: PlaylistEngine | null = null;
export function setEngineChoice(engine: PlaylistEngine | null) {
	engineChoice = engine;
}
/** Run `start` with `engine` as the choice for the writes it makes before its first `await` (where
 *  `transfer` calls `transferTracks`), then put the previous choice back. */
export function withEngineChoice<T>(engine: PlaylistEngine | null, start: () => Promise<T>): Promise<T> {
	const before = engineChoice;
	engineChoice = engine;
	try {
		return start();
	} finally {
		engineChoice = before;
	}
}
/** Alerts neither seen nor dismissed (the badge). */
export const unseenAlertCount = () => invoke<number>('unseen_alert_count');
/** The monitor page's cards. `items` is index rows (a track once per playlist); `duplicates_estimate`
 *  is the extra copies inside each synced playlist's newest snapshot, as PlaylistForge counts them. */
export type MonitorStats = {
	playlists: number;
	items: number;
	unavailable: number;
	duplicates_estimate: number;
};
export const monitorStats = () => invoke<MonitorStats>('monitor_stats');
export type MonitorOutcome = 'ok' | 'partial' | 'failed' | 'lock_busy' | 'cancelled';
/** One `monitor_runs` row. Times are epoch seconds; `detail_json` is the run's JSON detail as text. */
export type MonitorRun = {
	id: number;
	started_at: number;
	finished_at: number;
	trigger: SyncTrigger;
	outcome: MonitorOutcome;
	playlists_ok: number;
	playlists_failed: number;
	alerts_new: number;
	units_spent: number;
	detail_json: string;
};
/** The newest monitor runs, newest first (20 by default). */
export const monitorRuns = (limit?: number) =>
	invoke<MonitorRun[]>('monitor_runs', { limit: limit ?? null });
/** Every alert filed over the last `days` days (14 by default), oldest first, unbucketed: the page
 *  groups them by its own local day. */
export type AlertStamp = { at: number; kind: AlertKind };
export const alertsByDay = (days?: number) =>
	invoke<AlertStamp[]>('alerts_by_day', { days: days ?? null });
/** A sync's progress: `current` is the playlist being read, null once it is done. */
export type SyncProgress = { done: number; total: number; current: string | null };
export const onPlaylistSyncProgress = (cb: (p: SyncProgress) => void): Promise<UnlistenFn> =>
	listen<SyncProgress>('playlist-sync-progress', (e) => cb(e.payload));
/** A sync finished (any trigger, the scheduler's included): the index is worth re-reading. */
export const onPlaylistIndexSynced = (cb: (s: SyncSummary) => void): Promise<UnlistenFn> =>
	listen<SyncSummary>('playlist-index-synced', (e) => cb(e.payload));
export const onAlertsChanged = (cb: (unseen: number) => void): Promise<UnlistenFn> =>
	listen<{ unseen: number }>('alerts-changed', (e) => cb(e.payload.unseen));
/** Snapshot backups (backups.rs): the folder in use, the default one, and how many each playlist
 *  keeps. The folder and the count are the `monitor.backups_dir` and `retention_keep_last`
 *  settings; an empty folder setting means the default. `rejected`: the folder picked lies inside
 *  PlaylistForge's or LiMusic's data, so `dir` is the default instead. */
export type BackupsInfo = { dir: string; default_dir: string; keep: number; rejected: boolean };
export const backupsInfo = () => invoke<BackupsInfo>('backups_info');
export type BackupsOutcome = { written: number; pruned_files: number; pruned_rows: number };
/** Back up every synced playlist's newest snapshot now, then prune. Rejects with `busy` while a
 *  sync runs. */
export const exportBackupsNow = () => invoke<BackupsOutcome>('export_backups_now');
/** Open the backups folder in the file manager (created if missing). */
export const openBackupsDir = () => invoke<void>('open_backups_dir');

// --- downloads (Rust `download/`) ---------------------------------------------------------------
export type DownloadFormat = 'audio' | 'video';
export type DownloadStatus = 'queued' | 'running' | 'available' | 'error' | 'missing';
/** One `downloads` row. Dates are RFC 3339 UTC. */
export type DownloadRow = {
	video_id: string;
	format: DownloadFormat;
	status: DownloadStatus;
	requested_quality: string;
	thumbnail_mode: string;
	dest_dir: string;
	file_path: string | null;
	file_size_bytes: number | null;
	container: string | null;
	error: string | null;
	attempts: number;
	created_at: string;
	completed_at: string | null;
	last_verified_at: string | null;
};
/** yt-dlp's version (null: missing or broken), ffmpeg's presence, and whether the app installs
 *  them itself (Windows) or uses the ones on PATH. */
export type ToolsStatus = { ytdlp_version: string | null; ffmpeg_present: boolean; managed: boolean };
export const downloadToolsStatus = () => invoke<ToolsStatus>('download_tools_status');
/** Install or update yt-dlp (channel: the `downloads.ytdlp_channel` setting). Resolves with the
 *  installed version; rejects with `busy`, `not_managed`, `checksum_mismatch` or a message. */
export const installYtdlp = () => invoke<string>('install_ytdlp');
/** Install or update ffmpeg. Rejects as `installYtdlp`. */
export const installFfmpeg = () => invoke<void>('install_ffmpeg');
export type ToolsInstallProgress = {
	tool: 'yt-dlp' | 'ffmpeg';
	stage: 'resolving' | 'downloading' | 'verifying' | 'extracting' | 'done';
	received: number;
	total: number | null;
};
export const onToolsInstallProgress = (cb: (p: ToolsInstallProgress) => void): Promise<UnlistenFn> =>
	listen<ToolsInstallProgress>('tools-install-progress', (e) => cb(e.payload));
/** The download folder in use and the default one (the `downloads.dir` setting empty). */
export type DownloadsInfo = { dir: string; default_dir: string };
export const downloadsInfo = () => invoke<DownloadsInfo>('downloads_info');
export const openDownloadsDir = () => invoke<void>('open_downloads_dir');
/** `invalid`: songs left out because their id is not a YouTube video's. */
export type EnqueueResult = { queued: number; already: number; invalid: number };
/** What a download needs of a song: the id, plus what fills the catalog. A `SongItem` is one. */
export type DownloadSong = Pick<SongItem, 'video_id'> &
	Partial<Pick<SongItem, 'title' | 'artists' | 'duration'>>;
/** Queue songs. `format` defaults to the `downloads.default_format` setting; `redownload` queues
 *  downloaded ones again. Local files are skipped. */
export const downloadEnqueue = (
	songs: DownloadSong[],
	format?: DownloadFormat,
	redownload?: boolean
) =>
	invoke<EnqueueResult>('download_enqueue', {
		songs,
		format: format ?? null,
		redownload: redownload ?? null
	});
/** Stop the running download (its row goes). False when nothing runs. */
export const downloadCancel = () => invoke<boolean>('download_cancel');
export const downloadRetry = (videoId: string, format: DownloadFormat) =>
	invoke<boolean>('download_retry', { videoId, format });
/** Forget a download (stopping it if it runs). Never deletes the file. */
export const downloadRemove = (videoId: string, format: DownloadFormat) =>
	invoke<boolean>('download_remove', { videoId, format });
export type DownloadProgress = {
	video_id: string;
	format: DownloadFormat;
	percent: number;
	speed: string | null;
	eta: string | null;
};
export const downloadActive = () => invoke<DownloadProgress | null>('download_active');
/** The rows of these videos, by video, after checking their files are still on disk. */
export const downloadsFor = (videoIds: string[]) =>
	invoke<Record<string, DownloadRow[]>>('downloads_for', { videoIds });
export const downloadsRecent = (limit?: number) =>
	invoke<DownloadRow[]>('downloads_recent', { limit: limit ?? null });
/** The running download's progress; null when it ends. */
export const onDownloadProgress = (cb: (p: DownloadProgress | null) => void): Promise<UnlistenFn> =>
	listen<DownloadProgress | null>('download-progress', (e) => cb(e.payload));
/** How a finished run ended. `tools_missing`: queued downloads wait for yt-dlp. */
export type DownloadOutcome = {
	video_id: string;
	format: DownloadFormat;
	status: 'available' | 'error' | 'cancelled' | 'tools_missing';
	title: string | null;
	error: string | null;
};
/** Rows changed: these videos' (empty: possibly any), and the outcome of a finished run. */
export type DownloadsChanged = { video_ids: string[]; outcome: DownloadOutcome | null };
export const onDownloadsChanged = (cb: (c: DownloadsChanged) => void): Promise<UnlistenFn> =>
	listen<DownloadsChanged>('downloads-changed', (e) => cb(e.payload));
/**
 * videoId → times played, from the local listening history. Same trailing window On Repeat uses
 * (a month): the history table is pruned to it, so there is no older data. A videoId that isn't in
 * the map has not been played inside the window.
 */
export const getPlayCounts = () => invoke<Record<string, number>>('play_counts');
/**
 * `start`: the clicked track index, or `null` for "just play it" (random opener under shuffle).
 * `sourceId`: the page's playlist/album playlist id — makes autoplay continue with that
 * context's radio (omit to fall back to song radio seeded from the queue's last track).
 * `sourceName`: the page title, for the queue panel's "Next from" header.
 * `shuffle`: turn shuffle on for this queue — pass items in their real order, Rust shuffles.
 */
export const playPlaylist = (
	items: SongItem[],
	start: number | null,
	sourceId?: string,
	sourceName?: string,
	shuffle?: boolean,
	continuation?: string
) => invoke<void>('play_playlist', { items, start, sourceId, sourceName, shuffle, continuation });
/**
 * Start a radio: an endless YouTube-generated queue seeded on this item. `id` is the videoId
 * (song) or browseId/playlistId (everything else) — Rust resolves it to a radio playlist, so the
 * UI never builds one. `name` titles the queue ("<name> Radio").
 *
 * A song radio on the track that's already playing splices in behind it (no re-buffer); every
 * other case replaces the queue. Rejects when YouTube has no radio for the item.
 */
export const startRadio = (kind: 'song' | 'artist' | 'album' | 'playlist', id: string, name?: string) =>
	invoke<void>('start_radio', { kind, id, name });
export const getAlbum = (id: string) => invoke<AlbumPage>('get_album', { id });
export const getArtist = (id: string) => invoke<ArtistPage>('get_artist', { id });
/** A Moods & Genres tile. `params` browses `MOODS_CATEGORY_ID` into that mood's playlists. */
export interface Mood {
	title: string;
	params: string;
	/** YouTube's own colour for the tile, `#rrggbb`. */
	color: string;
}
export interface MoodSection {
	title: string;
	items: Mood[];
}
export const MOODS_CATEGORY_ID = 'FEmusic_moods_and_genres_category';
export const getMoods = () => invoke<MoodSection[]>('get_moods');
/** A cover per tile (the first playlist in its category), keyed by the tile's `params`. */
export const getMoodArt = (params: string[]) =>
	invoke<Record<string, string>>('get_mood_art', { params });
export const getBrowseGrid = (id: string, params?: string) =>
	invoke<BrowseItem[]>('get_browse_grid', { id, params });

// --- local music (local.rs) ------------------------------------------------------------------
/** Rescan the watched folders. Cheap when nothing changed (one stat per file). */
export const getLocalLibrary = () => invoke<LocalLibrary>('get_local_library');
export const addLocalFolder = (path: string) => invoke<LocalLibrary>('add_local_folder', { path });
export const removeLocalFolder = (path: string) =>
	invoke<LocalLibrary>('remove_local_folder', { path });

// --- blocked artists (blocked.rs, plan 046) ---------------------------------------------------
/** One entry in the block list. `id` is the channel browseId when the blocked row linked one. */
export interface BlockedArtist {
	id?: string;
	name: string;
}
export const getBlockedArtists = () => invoke<BlockedArtist[]>('get_blocked_artists');
/** Blocks the artist and returns the new list. Rust also drops them out of the live queue. */
export const blockArtist = (id: string | undefined, name: string) =>
	invoke<BlockedArtist[]>('block_artist', { id, name });
/** `key` is the entry's channel id when it has one, else its name. */
export const unblockArtist = (key: string) => invoke<BlockedArtist[]>('unblock_artist', { key });

// --- write actions (context/01 ✎) ----------------------------------------------------------
/** Like, dislike, or clear the rating. YouTube's three states are mutually exclusive, so a dislike
 *  un-likes in the same call. */
export const rate = (videoId: string, rating: Rating) => invoke<void>('rate', { videoId, rating });
/** `false` = the playlist already had this track, so YouTube added nothing. */
export const addToPlaylist = (playlistId: string, videoId: string, allowDuplicates = false) =>
	invoke<boolean>('add_to_playlist', { playlistId, videoId, allowDuplicates });
export const removeFromPlaylist = (playlistId: string, videoId: string, setVideoId: string) =>
	invoke<void>('remove_from_playlist', { playlistId, videoId, setVideoId });

/** Bulk removal: one request, all or nothing. `tracks` is [videoId, setVideoId] per row. */
export const removeManyFromPlaylist = (playlistId: string, tracks: [string, string][]) =>
	invoke<void>('remove_many_from_playlist', { playlistId, tracks });

// --- playlist tools (src-tauri/src/playlist_tools) ---------------------------------------------

/** A row going out of a playlist, with the handle of the row after it that stays: where an undo
 *  puts it back (`null`: the end). `song.set_video_id` is the row's own handle. */
export type RowRef = { song: SongItem; before: string | null };
/** One journal entry: an edit the playlist tools made, and whether it can still be undone. */
export type PlaylistOp = {
	id: number;
	kind: 'reorder' | 'remove' | 'copy' | 'move' | 'dedupe' | 'split' | 'merge' | 'extract' | 'add';
	summary: { playlists: { id: string; title: string }[]; count: number };
	createdAt: number;
	undone: boolean;
	undoable: boolean;
};
type RawPlaylistOp = Omit<PlaylistOp, 'createdAt'> & { created_at: number };
const op = (r: RawPlaylistOp): PlaylistOp => ({
	id: r.id,
	kind: r.kind,
	summary: r.summary,
	createdAt: r.created_at,
	undone: r.undone,
	undoable: r.undoable
});
/** Put a playlist in `order` (row handles). `null` when nothing had to move. */
export const reorderPlaylist = async (playlistId: string, title: string, order: string[]) => {
	const r = await invoke<RawPlaylistOp | null>('reorder_playlist', { playlistId, title, order });
	return r && op(r);
};
/** Take rows out of a playlist, undoably (see `RowRef`). `kind` names it in the history. */
export const removeTracks = async (
	playlistId: string,
	title: string,
	rows: RowRef[],
	kind: 'remove' | 'dedupe' = 'remove',
	engine?: PlaylistEngine
) => {
	const r = await invoke<RawPlaylistOp | null>('remove_tracks', {
		playlistId,
		title,
		rows: rows.map((r) => ({ song: r.song, before: r.before })),
		kind,
		engine: engine ?? engineChoice
	});
	return r && op(r);
};
/** Which copy of a duplicate stays: the highest, the lowest, or the audio track over a video. */
export type DuplicateKeep = 'first' | 'last' | 'prefer_song';
/** The whole playlist, and the groups of rows (indices into it) that are copies of each other. */
export type DuplicateReport = {
	rows: SongItem[];
	clusters: { rows: number[]; reasons: ('exact' | 'title' | 'similar')[]; keep: number }[];
};
export const findDuplicates = (
	playlistId: string,
	options: { exact: boolean; title: boolean; similar: boolean },
	keep: DuplicateKeep
) => invoke<DuplicateReport>('find_duplicates', { playlistId, options, keep });
/** What a copy or move did. `duplicates`: already in the target; `refused`: YouTube wouldn't take
 *  them (or files on this computer bound for an account playlist). */
export type Transferred = {
	added: number;
	duplicates: number;
	refused: number;
	removed: number;
	op: PlaylistOp | null;
};
/** Copy or move rows into `target`. `source` is null for a list that isn't a playlist of yours,
 *  which can only copy. See `playlist_tools/transfer.rs` for the duplicate policies. */
export const transferTracks = async (args: {
	source: { id: string; title: string } | null;
	target: { id: string; title: string };
	rows: RowRef[];
	mode: 'copy' | 'move';
	duplicates: 'skip' | 'allow' | 'consolidate';
	/** This write's engine; by default the confirmation's choice, else `playlist_engine`. */
	engine?: PlaylistEngine;
}) => {
	const r = await invoke<Omit<Transferred, 'op'> & { op: RawPlaylistOp | null }>('transfer_tracks', {
		source: args.source?.id ?? null,
		sourceTitle: args.source?.title ?? null,
		target: args.target.id,
		targetTitle: args.target.title,
		rows: args.rows.map((r) => ({ song: r.song, before: r.before })),
		mode: args.mode,
		duplicates: args.duplicates,
		engine: args.engine ?? engineChoice
	});
	return { ...r, op: r.op && op(r.op) };
};
/** Every row of a playlist in its order, read in full (every page of an account playlist). Rows of
 *  a playlist you can edit carry their handle (`set_video_id`); what Extract and Reorder work on. */
export const playlistRows = (playlistId: string) =>
	invoke<SongItem[]>('playlist_rows', { playlistId });
export type SplitBy = { by: 'artist'; min: number } | { by: 'count'; parts: number } | { by: 'size'; max: number };
export type SplitOrder =
	| { order: 'playlist' | 'title' | 'artist' | 'duration' }
	| { order: 'shuffle'; seed: number };
/** A split worked out but not written: the whole playlist and the rows (indices) of each part.
 *  `artist` names a per-artist part; null is the pooled one, or a numbered part. */
export type SplitPlan = { rows: SongItem[]; parts: { artist: string | null; rows: number[] }[] };
export const planSplit = (playlistId: string, by: SplitBy, order: SplitOrder) =>
	invoke<SplitPlan>('plan_split', { playlistId, by, order });
/** Several playlists merged into one list, not written. `into` is an existing target whose tracks
 *  a dedupe leaves out. */
export const mergePreview = (
	ids: string[],
	how: 'concat' | 'round_robin',
	dedupe: boolean,
	into: string | null
) => invoke<SongItem[]>('merge_preview', { ids, how, dedupe, into });
export type BuildDest = { to: 'new'; local: boolean } | { to: 'existing'; id: string; title: string };
export type Built = {
	created: { id: string; title: string }[];
	added: number;
	/** Rows a move took out of its source (an extract only). */
	removed: number;
	stopped: boolean;
	error: string | null;
	op: PlaylistOp | null;
};
/** Write a split, a merge or an extract. Progress comes through `onPlaylistOpProgress`. `mode`
 *  only matters to an extract from one playlist: `move` takes the rows out of it once they are in,
 *  undoably (copy when left out). */
export const buildPlaylists = async (args: {
	kind: 'split' | 'merge' | 'extract';
	sources: { id: string; title: string }[];
	lists: { name: string; songs: SongItem[] }[];
	dest: BuildDest;
	mode?: 'copy' | 'move';
}) => {
	const r = await invoke<Omit<Built, 'op'> & { op: RawPlaylistOp | null }>('build_playlists', {
		...args,
		mode: args.mode ?? null
	});
	return { ...r, op: r.op && op(r.op) };
};
export const cancelPlaylistBuild = () => invoke<void>('cancel_playlist_build');
/** Write the whole playlist to `path` as CSV, JSON or M3U8. Answers how many tracks went out. */
export const exportPlaylist = (
	playlistId: string,
	title: string,
	format: 'csv' | 'json' | 'm3u8',
	path: string
) => invoke<number>('export_playlist', { playlistId, title, format, path });
export type BuildProgress = { done: number; total: number; current: string };
export const onPlaylistOpProgress = (cb: (p: BuildProgress) => void): Promise<UnlistenFn> =>
	listen<BuildProgress>('playlist-op-progress', (e) => cb(e.payload));
/** A song in your playlists, once, with the ids of the playlists that hold it. */
/** A song with the playlists holding it, and when it was first seen in any of them (epoch seconds;
 *  null when they all held it from before tracking began). */
export type Everywhere = { song: SongItem; playlists: string[]; first_seen: number | null };
/** Every song across your playlists, from the index (no network). */
export const songsEverywhere = () => invoke<Everywhere[]>('songs_everywhere');
export type Kept = { added: number; removed: number; failed: string[]; op: PlaylistOp | null };
/** Keep `songs` only in `target` (added there if missing), or with no target, out of every
 *  playlist. `titles` names the playlists for the history. */
export const keepOnlyIn = async (
	songs: SongItem[],
	target: { id: string; title: string } | null,
	titles: Record<string, string>
) => {
	const r = await invoke<Omit<Kept, 'op'> & { op: RawPlaylistOp | null }>('keep_only_in', {
		songs,
		target,
		titles
	});
	return { ...r, op: r.op && op(r.op) };
};
export type AlertKind = 'added' | 'removed' | 'moved' | 'unavailable' | 'restored';
/** A change the monitor found in a playlist of yours since the sync before. `from`/`to` are the
 *  0-based positions a row went between, where known (always on `moved`). */
export type PlaylistAlert = {
	id?: number;
	playlist_id: string;
	video_id: string;
	kind: AlertKind;
	song: SongItem | null;
	at: number;
	seen: boolean;
	from?: number;
	to?: number;
	/** Dismissed from Library ▸ In your playlists (only ever true with `all`). */
	dismissed?: boolean;
};
/** The alerts not dismissed, newest first, one per playlist, track and kind (the newest of its
 *  repeats). `all` is the alerts page: every row ever filed, repeats and dismissed ones included,
 *  in pages of at most `limit` rows older than `before`, the `[at, id]` of the last row loaded.
 *  `limit`/`before` only apply with `all`. */
export const playlistAlerts = (
	opts: { all?: boolean; limit?: number; before?: [number, number] } = {}
) =>
	invoke<PlaylistAlert[]>('playlist_alerts', {
		all: opts.all ?? null,
		limit: opts.limit ?? null,
		before: opts.before ?? null
	});
/** Mark these alerts seen, or every one with no ids. Answers the unseen count after (also sent as
 *  `alerts-changed`). */
export const markAlertsSeen = (ids?: number[]) =>
	invoke<number>('mark_alerts_seen', { ids: ids ?? null });
/** One change between two snapshots of a playlist; `from`/`to` are 0-based positions in the older
 *  and the newer one, where known. */
export type TimelineChange = {
	video_id: string;
	kind: AlertKind;
	song: SongItem | null;
	from?: number;
	to?: number;
};
/** One snapshot of a playlist and what changed since the one before it. `baseline` is the oldest
 *  kept, with nothing to compare against. */
export type TimelineEntry = {
	snapshot_id: number;
	taken_at: number;
	item_count: number;
	title: string | null;
	baseline: boolean;
	added: number;
	removed: number;
	moved: number;
	unavailable: number;
	restored: number;
	changes: TimelineChange[];
};
/** Every snapshot kept of a playlist, newest first, with its changes. Empty when never synced. */
export const playlistTimeline = (playlistId: string) =>
	invoke<TimelineEntry[]>('playlist_timeline', { playlistId });
export const dismissPlaylistAlert = (a: PlaylistAlert) =>
	invoke<void>('dismiss_playlist_alert', {
		playlistId: a.playlist_id,
		videoId: a.video_id,
		kind: a.kind
	});
export const playlistHistory = async () =>
	(await invoke<RawPlaylistOp[]>('playlist_history')).map(op);
export const undoPlaylistOp = async (id: number) =>
	op(await invoke<RawPlaylistOp>('undo_playlist_op', { id }));
/** A playlist tool (or its undo) changed these playlists: a page showing one should re-read it. */
export const onPlaylistsEdited = (cb: (ids: string[]) => void): Promise<UnlistenFn> =>
	listen<string[]>('playlists-edited', (e) => cb(e.payload));

/** `local` keeps it on this machine instead of the account, the only kind there is signed out.
 *  Answers the new id: a `LOCALPLAYLIST:` browseId for a local one, YouTube's playlist id else. */
export const createPlaylist = (title: string, local = false) =>
	invoke<string>('create_playlist', { title, local });
/** Add whole songs to a playlist on this machine, in one write. Answers per song whether it went
 *  in: `false` means the playlist already had it. Local files are fine here. */
export const addToLocalPlaylist = (playlistId: string, items: SongItem[]) =>
	invoke<boolean[]>('add_to_local_playlist', { playlistId, items });
/** Name / description / visibility, from the "Edit playlist" dialog. Leave a field out and
 *  YouTube is never told about it, so an untouched one can't be overwritten. */
export const editPlaylistDetails = (
	playlistId: string,
	changes: { name?: string; description?: string; public?: boolean }
) => invoke<void>('edit_playlist_details', { playlistId, ...changes });
/** Custom playlist artwork. `path` is a file the user picked; `null` drops it. Answers where the
 *  local copy went, and on a removal the thumbnail YouTube rebuilt from the tracks (that one is
 *  worth waiting for: YouTube's own thumbnail is the cover being removed until it lands). */
export const setPlaylistCover = (playlistId: string, path: string | null) =>
	invoke<{ cover?: string; thumbnail?: string }>('set_playlist_cover', { playlistId, path });
export const deletePlaylist = (playlistId: string) =>
	invoke<void>('delete_playlist', { playlistId });
export const subscribe = (channelId: string, subscribed: boolean) =>
	invoke<void>('subscribe', { channelId, subscribed });
/** Add a song to Library ▸ Songs, or take it out. `token` is `SongItem.library.add_token` /
 *  `.remove_token`; YouTube mints them per row, so they come from the list the song was shown in. */
export const setSongSaved = (token: string) => invoke<void>('set_song_saved', { token });
/** Save an album to the library (or remove it). `playlistId` is `AlbumPage.playlistId`. */
export const setAlbumSaved = (playlistId: string, saved: boolean) =>
	invoke<void>('set_album_saved', { playlistId, saved });

// --- Spotify import (spotify.rs, import.rs, #375) ----------------------------------------------
// Rejections are short codes (`private`, `busy`, ...) worded by `importError` in import.svelte.ts.

export type ImportTier = 'pending' | 'matched' | 'check' | 'missing';
export type ImportPhase = 'matching' | 'review' | 'creating' | 'done' | 'failed' | 'cancelled';
/** `liked` is Liked Songs, named in the user's language on this side. */
export type ImportListKind = 'playlist' | 'album' | 'liked';

export interface ImportListPreview {
	kind: ImportListKind;
	name: string;
	owner?: string | null;
	cover?: string | null;
	count: number;
	/** Rows that aren't songs (podcast episodes), left out. */
	skipped: number;
	/** Spotify only let the first 100 tracks be read. */
	truncated: boolean;
}
export interface ImportPreview {
	lists: ImportListPreview[];
}
export interface ImportResult {
	kind: ImportListKind;
	name: string;
	/** The browse id to open: `VL…`, or `LOCALPLAYLIST:<n>` on this device. */
	id: string;
	local: boolean;
	added: number;
	missing: number;
	removed: number;
}
export interface ImportSnapshot {
	phase: ImportPhase;
	/** Distinct tracks across every list being imported. */
	total: number;
	done: number;
	matched: number;
	check: number;
	missing: number;
	lists: { kind: ImportListKind; name: string; count: number; cover?: string | null }[];
	/** The last few tracks matched, newest first. */
	recent: { title: string; artists: string; tier: ImportTier; thumbnail?: string | null }[];
	/** While creating: steps done, steps in all. */
	step: [number, number];
	/** Pausing to stay within the hourly search budget: the unix second it goes on. */
	waitingUntil?: number | null;
	message?: string | null;
	results: ImportResult[];
	/** Set when this is an "Update from Spotify" of that playlist rather than an import. */
	update?: string | null;
}
export interface ImportRow {
	key: string;
	title: string;
	artists: string;
	album?: string | null;
	durationMs?: number | null;
	tier: ImportTier;
	pick?: SongItem | null;
	/** Empty for matched rows. */
	candidates: SongItem[];
}
export type ImportResolved =
	| { kind: 'song'; song: SongItem }
	| { kind: 'album'; id: string }
	| { kind: 'artist'; id: string }
	| { kind: 'playlist' };

export const importReadLink = (link: string) => invoke<ImportPreview>('import_read', { link });
export const importReadPath = (path: string) => invoke<ImportPreview>('import_read', { path });
/** A dropped file: the bytes go over as the raw request body (a webview drop has no path). */
export const importReadFile = async (file: File) =>
	invoke<ImportPreview>('import_read_file', new Uint8Array(await file.arrayBuffer()), {
		headers: { 'x-file-name': encodeURIComponent(file.name) }
	});
/** `lists` are indices into the last preview. */
export const importStart = (lists: number[]) => invoke<ImportSnapshot>('import_start', { lists });
export const importStatus = () => invoke<ImportSnapshot | null>('import_status');
export const importRows = (tier: ImportTier) => invoke<ImportRow[]>('import_rows', { tier });
/** `null` leaves the track out. A song picked here is remembered for every later import. */
export const importPick = (key: string, song: SongItem | null) =>
	invoke<ImportSnapshot>('import_pick', { key, song });
export const importCreate = (options: { names?: Record<number, string>; local?: boolean }) =>
	invoke<void>('import_create', { options });
/** Stops a running import, or puts away a finished one. */
export const importCancel = () => invoke<void>('import_cancel');
export const importSource = (playlistId: string) =>
	invoke<string | null>('import_source', { playlistId });
export const importUpdate = (playlistId: string) =>
	invoke<ImportSnapshot>('import_update', { playlistId });
export const importResolve = (link: string) => invoke<ImportResolved>('import_resolve', { link });

// --- events (context/11). Each returns an unlisten fn; call it on component teardown. --------
export const onNowPlaying = (cb: (n: NowPlaying) => void): Promise<UnlistenFn> =>
	listen<NowPlaying>('now-playing', (e) => cb(e.payload));
/**
 * The backend asked YouTube what a track's rating really is and got a different answer than the
 * row we were handed (issue #93). Fires only on a change, at most once per track start.
 */
export const onRating = (cb: (videoId: string, rating: Rating) => void): Promise<UnlistenFn> =>
	listen<{ videoId: string; rating: Rating }>('rating', (e) =>
		cb(e.payload.videoId, e.payload.rating)
	);
/** Linux and Windows: mpv has this track's music video (or will as soon as the track starts). */
export const onVideoReady = (cb: (videoId: string) => void): Promise<UnlistenFn> =>
	listen<string>('video-ready', (e) => cb(e.payload));
export const onQueueChanged = (cb: (q: QueueState) => void): Promise<UnlistenFn> =>
	listen<QueueState>('queue-changed', (e) => cb(e.payload));
/**
 * The queue moved but its track list did not: only the play pointer and the flags changed.
 * Emitted instead of `queue-changed` on every advance and skip, because the full item list is
 * megabytes on a big playlist and a Tauri event delivers its payload as JavaScript *source*.
 * `current` carries the playing row so a metadata backfill (duration, artists) still lands.
 */
export interface QueueIndex {
	currentIndex: number;
	shuffle?: boolean;
	repeat?: RepeatMode;
	sourceName?: string | null;
	sourceId?: string | null;
	prevTrack?: string | null;
	current: SongItem | null;
}

export const onQueueIndex = (cb: (q: QueueIndex) => void): Promise<UnlistenFn> =>
	listen<QueueIndex>('queue-index', (e) => cb(e.payload));
/**
 * Autoplay topped the queue up at the tail. Carries only the new rows plus the resulting length, so
 * an endless radio session does not re-ship the whole list (which a Tauri event delivers as
 * JavaScript *source*) every twenty tracks. `len` is the resync guard: if the array we hold does
 * not reach that length once the rows are appended, an event was missed and the panel refetches.
 */
export interface QueueAppended {
	items: SongItem[];
	len: number;
	currentIndex: number;
}

export const onQueueAppended = (cb: (q: QueueAppended) => void): Promise<UnlistenFn> =>
	listen<QueueAppended>('queue-appended', (e) => cb(e.payload));
/** Main window shown/hidden (close-to-tray, the mini player). WebKitGTK never tells the page. */
export const onUiVisible = (cb: (v: boolean) => void): Promise<UnlistenFn> =>
	listen<boolean>('ui-visible', (e) => cb(e.payload));
/** `limusic-forge <link>` (#348): the arguments a cold launch was given, handed over once... */
export const takeLaunchArgs = () => invoke<string[]>('take_launch_args');
/** ...and those of a second launch while this one runs. */
export const onOpenLink = (cb: (args: string[]) => void): Promise<UnlistenFn> =>
	listen<string[]>('open-link', (e) => cb(e.payload));
export const onPosition = (cb: (p: number) => void): Promise<UnlistenFn> =>
	listen<{ position: number }>('position', (e) => cb(e.payload.position));
export const onDuration = (cb: (d: number) => void): Promise<UnlistenFn> =>
	listen<{ duration: number }>('duration', (e) => cb(e.payload.duration));
/** Echo of every `set_volume`, so a second window's slider can't drift from what you hear. */
export const onVolume = (cb: (v: number) => void): Promise<UnlistenFn> =>
	listen<number>('volume', (e) => cb(e.payload));
export const onPlaybackState = (cb: (s: 'playing' | 'paused') => void): Promise<UnlistenFn> =>
	listen<'playing' | 'paused'>('playback-state', (e) => cb(e.payload));
export const onPlaybackError = (cb: (msg: string) => void): Promise<UnlistenFn> =>
	listen<{ message: string }>('playback-error', (e) => cb(e.payload.message));
export const onPlaybackNotice = (cb: (msg: string) => void): Promise<UnlistenFn> =>
	listen<{ message: string }>('playback-notice', (e) => cb(e.payload.message));
/** Custom playlist artwork applied here but refused by YouTube Music (it syncs in the background,
 *  so the failure lands long after the picker closed). */
export const onCoverError = (cb: (msg: string) => void): Promise<UnlistenFn> =>
	listen<{ message: string }>('cover-error', (e) => cb(e.payload.message));
export const onImportProgress = (cb: (s: ImportSnapshot) => void): Promise<UnlistenFn> =>
	listen<ImportSnapshot>('import-progress', (e) => cb(e.payload));
export const onAuthChanged = (cb: (a: Account) => void): Promise<UnlistenFn> =>
	listen<Account>('auth-changed', (e) => cb(e.payload));
export const onAccountSelectionRequired = (cb: () => void): Promise<UnlistenFn> =>
	listen('account-selection-required', () => cb());
/**
 * Local music disappeared from disk. Fired when a play attempt finds nothing there, carrying the
 * song (and album, if that emptied it) so every view holding those ids can drop them at once.
 */
export const onLocalChanged = (cb: (removed: string[]) => void): Promise<UnlistenFn> =>
	listen<{ removed: string[] }>('local-changed', (e) => cb(e.payload.removed));
export const onLoginError = (cb: (msg: string) => void): Promise<UnlistenFn> =>
	listen<string>('login-error', (e) => cb(e.payload));
export const onLoginDone = (cb: () => void): Promise<UnlistenFn> =>
	listen('login-done', () => cb());

// --- lyrics ---------------------------------------------------------------------------------
export interface LyricWord {
	text: string;
	start_ms: number;
	end_ms: number;
}
export interface LyricLine {
	/** Start cue in milliseconds; present ⇔ the line is synced. */
	time_ms?: number;
	end_time_ms?: number;
	text: string;
	words?: LyricWord[];
	translation?: string;
	/** Latin-script reading of `text` (#202). Word-timed only when Apple Music wrote it. */
	romanized?: string;
	romanized_words?: LyricWord[];
}
export interface Lyrics {
	/** Attribution for the panel footer ("LRCLIB", "Source: Musixmatch", …). */
	source: string;
	/** Id of the provider that answered (`LyricsProvider.id`). */
	provider: string;
	synced: boolean;
	instrumental: boolean;
	lines: LyricLine[];
	/** The source was picked by hand for this song (or its timing nudged). */
	pinned: boolean;
	/** Timing nudge in ms, positive = lyrics later. */
	offset_ms: number;
}
// A type, not an interface: `invoke` takes a record, and only a type alias is assignable to one.
export type LyricsTrack = {
	videoId: string;
	title: string;
	artists: string;
	album?: string;
	duration?: number;
};
/** Cached on the Rust side, down the user's provider order. `null` = none found. `source` asks that
 *  one provider alone and caches nothing (the source picker's preview); it rejects when the
 *  provider couldn't be reached, which is not the same as it having no lyrics. */
export const getLyrics = (args: LyricsTrack & { source?: string }) =>
	invoke<Lyrics | null>('get_lyrics', args);
/** Keep `source`'s lyrics for this song, or (`null`) hand it back to the provider order. */
export const chooseLyricsSource = (args: LyricsTrack & { source: string | null }) =>
	invoke<Lyrics | null>('choose_lyrics_source', args);
export const setLyricsOffset = (videoId: string, offsetMs: number) =>
	invoke<void>('set_lyrics_offset', { videoId, offsetMs });
export interface LyricsProvider {
	id: string;
	name: string;
	on: boolean;
}
/** Every provider in the user's order (setting `lyrics_providers`: ids, `-id` switched off). */
export const lyricsProviders = () => invoke<LyricsProvider[]>('lyrics_providers');

// --- Window ------------------------------------------------------------------------------------
/** Theater mode's fullscreen. Not `getCurrentWindow().setFullscreen` (#139): Windows needs the
 *  maximized state undone first and the frame recalculated after, in that order, on the main
 *  thread. Rust also puts the maximized state back when theater closes. */
export const theaterFullscreen = (on: boolean) => invoke<void>('theater_fullscreen', { on });

// --- Last.fm scrobbling ---------------------------------------------------------------------
export interface LastfmState {
	/** From `lastfm_status` only: whether this build carries Last.fm API credentials. */
	configured?: boolean;
	connected: boolean;
	username?: string | null;
	/** Set when a connect attempt failed (timeout, network, rejected) — show it as a toast. */
	error?: string | null;
}
export const lastfmStatus = () => invoke<LastfmState>('lastfm_status');
/** Opens the browser auth flow; the outcome arrives via onLastfmState, not this promise. */
export const lastfmConnect = () => invoke<void>('lastfm_connect');
/** Also cancels an in-flight connect (the auth poll checks and bails). */
export const lastfmDisconnect = () => invoke<void>('lastfm_disconnect');
export const onLastfmState = (cb: (s: LastfmState) => void): Promise<UnlistenFn> =>
	listen<LastfmState>('lastfm-state', (e) => cb(e.payload));

// --- Listen Together (context/19) -----------------------------------------------------------
export interface LtUser {
	user_id: string;
	username: string;
	is_host: boolean;
	is_connected: boolean;
}
export interface LtTrack {
	id: string;
	title: string;
	artist: string;
	thumbnail?: string | null;
	duration_ms: number;
	/** Name of the guest who added this track to the session queue. */
	queued_by?: string | null;
}
export interface LtPendingJoin {
	userId: string;
	username: string;
}
export interface LtSuggestion {
	id: string;
	from_user_id: string;
	from_username: string;
	track: LtTrack;
}
export interface LtState {
	status: 'disconnected' | 'connecting' | 'connected';
	role: 'none' | 'host' | 'guest';
	/** Asked to create/join and awaiting the room (host approval) — show a waiting state. */
	requesting: boolean;
	roomCode: string | null;
	myId: string | null;
	/** Empty means the built-in default; the backend resolves it when it connects. */
	serverUrl: string;
	/** What that default is. Only ever rendered inside the "change server" panel. */
	defaultServerUrl: string;
	users: LtUser[];
	currentTrack: LtTrack | null;
	queue: LtTrack[];
	pendingJoins: LtPendingJoin[];
	suggestions: LtSuggestion[];
}

export const ltGetState = () => invoke<LtState>('lt_get_state');
export const ltSetServerUrl = (url: string) => invoke<void>('lt_set_server_url', { url });
export const ltCreateRoom = (username: string) => invoke<void>('lt_create_room', { username });
export const ltJoinRoom = (code: string, username: string) =>
	invoke<void>('lt_join_room', { code, username });
export const ltLeave = () => invoke<void>('lt_leave');
export const ltApproveJoin = (userId: string) => invoke<void>('lt_approve_join', { userId });
export const ltRejectJoin = (userId: string) => invoke<void>('lt_reject_join', { userId });
export const ltKick = (userId: string) => invoke<void>('lt_kick', { userId });
export const ltTransferHost = (userId: string) => invoke<void>('lt_transfer_host', { userId });
export const ltApproveSuggestion = (id: string) => invoke<void>('lt_approve_suggestion', { id });
export const ltRejectSuggestion = (id: string) => invoke<void>('lt_reject_suggestion', { id });
export const ltRequestSync = () => invoke<void>('lt_request_sync');

export const onLtState = (cb: (s: LtState) => void): Promise<UnlistenFn> =>
	listen<LtState>('lt-state', (e) => cb(e.payload));
export const onLtNotice = (cb: (msg: string) => void): Promise<UnlistenFn> =>
	listen<string>('lt-notice', (e) => cb(e.payload));

// --- jobs page ---
// The playlist job queue and its history (src-tauri/src/jobs/control.rs), today's Data API quota
// and its last two weeks. Dates are RFC 3339 UTC.
export type JobStatus =
	| 'queued'
	| 'running'
	| 'paused_user'
	| 'waiting_quota'
	| 'waiting_auth'
	| 'paused_network'
	| 'verifying'
	| 'completed'
	| 'completed_with_errors'
	| 'failed'
	| 'cancelled';
export type JobEngine = 'ytdata' | 'innertube';
/** One job as the jobs page shows it. `priority`: 0 the app's own, 1 high, 2 normal, 3 low. */
export type JobView = {
	id: number;
	kind: string;
	/** The operation asked for (`copy`, `move`, `dedupe`...), when the dispatch named it. */
	op_kind: string | null;
	status: JobStatus;
	priority: number;
	engine: JobEngine;
	account_id: string | null;
	phase: number;
	total_phases: number;
	created_at: string;
	started_at: string | null;
	finished_at: string | null;
	resume_at: string | null;
	est_units_total: number;
	spent_units: number;
	total_items: number;
	done_items: number;
	failed_items: number;
	skipped_items: number;
	/** The tracks asked for; unlike `total_items` it does not grow with a move's later phases. */
	planned_items: number;
	retried_items: number;
	last_error: string | null;
	summary: { playlists: { id: string; title: string }[]; count: number } | null;
	undo_of_job_id: number | null;
	revert_job_id: number | null;
	revert_requested: boolean;
	/** Its entry in the undo history (`playlistHistory`). */
	op_id: number | null;
};
export type JobItemStatus = 'pending' | 'in_flight' | 'done' | 'failed' | 'skipped';
export type JobItemView = {
	id: number;
	seq: number;
	phase: number;
	action: string;
	status: JobItemStatus;
	attempts: number;
	last_error: string | null;
	video_id: string | null;
	playlist_id: string | null;
	updated_at: string;
};
export type JobDetail = {
	job: JobView;
	items: JobItemView[];
	/** What undoing it would take: the done items with an inverse, and their cost on the Data API. */
	revert: { item_count: number; estimated_units: number };
};
/** `active`: not ended, in the order they run. `history`: ended, newest first. `all`: both. */
export type JobsFilter = 'active' | 'history' | 'all';
export const jobsList = (filter?: JobsFilter, limit?: number) =>
	invoke<JobView[]>('jobs_list', { filter: filter ?? null, limit: limit ?? null });
export const jobDetail = (id: number) => invoke<JobDetail | null>('job_detail', { id });
export const jobPause = (id: number) => invoke<void>('job_pause', { id });
export const jobResume = (id: number) => invoke<void>('job_resume', { id });
/** Cancel; with `revert`, also undo what it did (on an ended job that is the whole request). */
export const jobCancel = (id: number, revert: boolean) => invoke<void>('job_cancel', { id, revert });
/** Put its failed items back in the queue. Answers how many. */
export const jobRetryFailed = (id: number) => invoke<number>('job_retry_failed', { id });
/** 1 high, 2 normal, 3 low. */
export const jobSetPriority = (id: number, priority: number) =>
	invoke<void>('job_set_priority', { id, priority });
/** The queue in a new order. Jobs keep their priority; within one they run in this order. */
export const jobsReorder = (ids: number[]) => invoke<number>('jobs_reorder', { ids });
export type EndpointUsage = { endpoint: string; calls: number; units: number };
export type QuotaToday = {
	spent: number;
	daily_units: number;
	next_reset: string;
	/** Costliest first. */
	endpoints: EndpointUsage[];
};
export const quotaToday = () => invoke<QuotaToday>('quota_today');
/** One Pacific day's spend; `date` is `YYYY-MM-DD`. */
export type DailyUsage = { date: string; units: number };
/** The last `days` Pacific days (14 by default), oldest first, today last. */
export const quotaHistory = (days?: number) =>
	invoke<DailyUsage[]>('quota_history', { days: days ?? null });
/** Today's quota as the budget bar splits it. Units; `daily_units` is the whole bar. */
export type BudgetPartition = {
	daily_units: number;
	spent_total: number;
	spent_backup: number;
	spent_jobs: number;
	spent_other: number;
	reserve_remaining: number;
	safety_margin: number;
	available_for_jobs: number;
	next_reset: string;
};
export const budgetPartition = () => invoke<BudgetPartition>('budget_partition');
/** A job changed; `jobId` 0 means possibly any. */
export const onJobsChanged = (cb: (jobId: number) => void): Promise<UnlistenFn> =>
	listen<{ job_id?: number } | null>('jobs-changed', (e) => cb(e.payload?.job_id ?? 0));
/** Data API units were spent. */
export const onQuotaChanged = (cb: () => void): Promise<UnlistenFn> =>
	listen('quota-changed', () => cb());

// --- headless monitor and Windows task ---
// The Windows scheduled task that runs `limusic-forge --monitor --all` daily (src-tauri/src/
// wintask.rs). Dates are as Windows printed them, in its own language and format.
export type WinTaskStatus = {
	/** `false` off Windows: there is no task to register. */
	supported: boolean;
	registered: boolean;
	/** `HH:MM`, local: the time registering uses (`monitor.schedule_time`). */
	schedule_time: string;
	next_run: string | null;
	status: string | null;
	last_run: string | null;
	last_result: string | null;
	/** The exe the task runs, and this one's. */
	registered_exe: string | null;
	current_exe: string | null;
	/** The task runs another copy (a portable folder that moved): register again to fix it. */
	exe_moved: boolean;
	/** Why the Task Scheduler could not be asked. */
	error: string | null;
};
export const wintaskStatus = () => invoke<WinTaskStatus>('wintask_status');
/** Register (or move) the task to run this copy daily at `time` (`HH:MM`). */
export const wintaskRegister = (time: string) => invoke<WinTaskStatus>('wintask_register', { time });
export const wintaskUnregister = () => invoke<WinTaskStatus>('wintask_unregister');

// --- PlaylistForge import ---
// Settings ▸ Import & migrate ▸ PlaylistForge (src-tauri/src/pf_import/). Rejections are short
// codes: `pf_running` (close PlaylistForge first), `not_found`, `not_playlistforge`, `too_new`,
// `corrupt`, `io`, `sqlite`, `consent_required`, `confirmation_required`.
export type PfDetect = {
	/** PlaylistForge's folder, when it holds a database. */
	path: string | null;
	found: boolean;
	running: boolean;
	user_version: number | null;
	too_new: boolean;
	/** "PlaylistForge Monitor" is registered; null off Windows or when Windows did not answer. */
	task: boolean | null;
	error: string | null;
};
export type PfSummary = {
	user_version: number;
	accounts: number;
	playlists: number;
	playlists_with_items: number;
	items: number;
	snapshots: number;
	alerts: number;
	downloads: number;
	jobs_pending: number;
	jobs_total: number;
	quota_units_today: number;
	settings: number;
	backups: number;
	has_client_secret: boolean;
	json_accounts: number;
};
/** D36: whose index wins when the playlist goes into the index. */
export type PfWinner = 'pf' | 'forge';
export type PfPreviewPlaylist = {
	id: string;
	title: string;
	account_id: string;
	privacy: string;
	items: number;
	deleted_remotely: boolean;
	in_forge: boolean;
	forge_synced_at: number | null;
	pf_synced_at: string | null;
	winner: PfWinner;
};
export type PfPreviewAccount = {
	id: string;
	title: string;
	last_sync_at: string | null;
	/** The Forge cookie account of the same channel, if one is saved. */
	forge_account: string | null;
	/** PlaylistForge has it as a Data API account (a token may be there to bring over). */
	data_api: boolean;
};
export type PfForgeAccount = {
	id: string;
	name: string | null;
	channel_id: string | null;
	active: boolean;
};
export type PfThemeChoice = { id: string; mode: 'dark' | 'light' | null };
export type PfPreview = {
	summary: PfSummary;
	playlists: PfPreviewPlaylist[];
	accounts: PfPreviewAccount[];
	forge_accounts: PfForgeAccount[];
	pf_client_secret: boolean;
	forge_client_secret: boolean;
	tokens_compatible: boolean;
	theme: PfThemeChoice | null;
	locale: string | null;
	problems: string[];
};
export type PfMissingAs = 'local' | 'account' | 'skip';
export type PfSelection = {
	path?: string | null;
	playlists: string[];
	/** PlaylistForge channel id → Forge cookie account id (null: none). Left out: paired automatically. */
	account_map: Record<string, string | null>;
	missing_as: PfMissingAs;
	alerts: boolean;
	downloads: boolean;
	settings: boolean;
	appearance: boolean;
	data_api: boolean;
};
export type PfReport = {
	playlists_indexed: number;
	playlists_kept: number;
	playlists_history: number;
	local_created: number;
	local_existing: number;
	account_queued: number;
	missing_skipped: number;
	tracks: number;
	snapshots: number;
	alerts: number;
	videos: number;
	downloads: number;
	downloads_available: number;
	downloads_missing: number;
	settings: number;
	settings_skipped: { key: string; reason: string }[];
	theme: PfThemeChoice | null;
	locale: string | null;
	ytdata_accounts: number;
	jobs: number;
	job_items: number;
	quota_entries: number;
	client_secret_copied: boolean;
	warnings: string[];
};
export type PfApplyResult = {
	report: PfReport;
	/** The account playlists went to the import queue (followed by `import-progress`). */
	import_started: boolean;
	import_error: string | null;
};
export type PfTokenOutcome = {
	channel_id: string;
	outcome: 'imported' | 'none' | 'error';
	code: string | null;
};
export type PfProgress = { step: string; done: number; total: number };
export const pfDetect = () => invoke<PfDetect>('pf_detect');
export const pfPreview = (path?: string | null) => invoke<PfPreview>('pf_preview', { path: path ?? null });
export const pfImportApply = (selection: PfSelection) =>
	invoke<PfApplyResult>('pf_import_apply', { selection });
/** Only with `consent` (the box the user ticked); Rust refuses otherwise. */
export const pfImportCredentials = (channelIds: string[], consent: boolean, path?: string | null) =>
	invoke<PfTokenOutcome[]>('pf_import_credentials', { channelIds, consent, path: path ?? null });
/** Only after the user confirmed: `schtasks /delete /tn "PlaylistForge Monitor" /f`. */
export const pfUnregisterTask = (confirmed: boolean) =>
	invoke<void>('pf_unregister_task', { confirmed });
export const onPfImportProgress = (cb: (p: PfProgress) => void): Promise<UnlistenFn> =>
	listen<PfProgress>('pf-import-progress', (e) => cb(e.payload));
