<script module lang="ts">
	// Module scope, so returning to the library (back from an album you opened, or via the sidebar)
	// keeps the tab you were on instead of snapping to All.
	let lastTab = 'all';
</script>

<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { page } from '$app/state';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		Add01Icon,
		CloudSyncIcon,
		CloudUploadIcon,
		DashboardSquare02Icon,
		DriveIcon,
		RefreshIcon,
		Sorting01Icon,
		MusicNote01Icon,
		MusicNoteSquare02Icon,
		Playlist02Icon,
		SpotifyIcon,
		SquareStackIcon,
		UserCheck01Icon,
		UserSharingIcon,
		LeftToRightListBulletIcon
	} from '@hugeicons/core-free-icons';
	import { Button } from '$lib/components/ui/button';
	import * as Tabs from '$lib/components/ui/tabs';
	import * as Tooltip from '$lib/components/ui/tooltip';
	import LibrarySongs from '$lib/components/LibrarySongs.svelte';
	import PlaylistSongs from '$lib/components/PlaylistSongs.svelte';
	import * as api from '$lib/api';
	import LocalMusic from '$lib/components/LocalMusic.svelte';
	import MediaCard from '$lib/components/MediaCard.svelte';
	import MediaCardSkeleton from '$lib/components/MediaCardSkeleton.svelte';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import ItemMenu from '$lib/components/ItemMenu.svelte';
	import * as Select from '$lib/components/ui/select';
	import type { BrowseItem, PlaylistSyncInfo } from '$lib/api';
	import {
		auth,
		monitor,
		personal,
		toast,
		library,
		loadLibrary,
		loadLibraryExtras,
		loadUploadAlbums,
		openNewPlaylist,
		syncAllPlaylists,
		syncSavedToYouTube
	} from '$lib/player.svelte';
	import {
		DEFAULT_SORT,
		DEFAULT_VIEW,
		formatSort,
		infoFor,
		parseSort,
		parseView,
		relativeAgo,
		sortPlaylists,
		syncLine,
		type PlView
	} from '$lib/plsort';
	import { openItem } from '$lib/browse';
	import { thumb } from '$lib/thumb';
	import { mergeSaved, unsynced } from '$lib/personal';
	import { reveal } from '$lib/reveal.svelte';
	import { t } from '$lib/i18n.svelte';
	import { openImport } from '$lib/import.svelte';
	import { isResolved } from '$lib/alerts';
	import ExperimentalBadge from '$lib/components/ExperimentalBadge.svelte';

	// `?tab=local` so anything that sends you back here (an album whose files were deleted) lands
	// on the tab you came from instead of a sign-in prompt.
	let tab = $state(page.url.searchParams.get('tab') ?? lastTab);
	$effect(() => {
		lastTab = tab;
	});
	// And on every later navigation to `/library?tab=…` too: SvelteKit keeps this page mounted when
	// only the query changes, so reading it once at mount left an already-open Library where it was.
	$effect(() => {
		const wanted = page.url.searchParams.get('tab');
		if (wanted) untrack(() => (tab = wanted));
	});
	// Uploads splits three ways (all / songs / albums): YouTube Music takes uploaded albums too, and
	// they are a card grid rather than rows, so they can't just join the track list.
	let uploadTab = $state('all');
	// Tracks the monitor flagged (`playlist_tools/monitor.rs`), counted on the tab so they are seen
	// without opening it.
	let alertCount = $state(0);
	onMount(() => {
		api.playlistAlerts()
			.then((a) => (alertCount = a.filter((x) => !isResolved(x)).length))
			.catch(() => {});
	});
	// Artists splits like YouTube Music's: the artists behind your songs, or the ones you subscribe
	// to. Only the second can be taken out of the library (an unsubscribe); the first leaves with
	// its songs.
	let artistTab = $state('artists');

	// Everything here lives in the shared `library` store, so a revisit renders the cached grid
	// immediately and the forced refresh below swaps in fresh data behind it. What was saved on this
	// machine merges in per tab (`mergeSaved`), which is the whole library when signed out.
	const playlists = $derived(mergeSaved(personal, library.items, 'playlist'));
	const albums = $derived(mergeSaved(personal, library.albums, 'album'));
	const artists = $derived(mergeSaved(personal, library.artists, 'artist'));
	const subscriptions = $derived(mergeSaved(personal, library.subscriptions, 'artist'));
	const all = $derived([...playlists, ...albums, ...artists]);
	// One per tab rather than one shared instance reset on switch: an `$effect` reset lands
	// after the render it is meant to govern, so switching tabs would build the new tab's grid
	// against the old tab's count and immediately tear the excess back down. A tab keeping its
	// own depth also means coming back to one lands where you left it.
	const rvAll = reveal();
	const rvPlaylists = reveal();
	const rvAlbums = reveal();
	const rvArtists = reveal();
	const rvSubscriptions = reveal();
	const rvUploadsAll = reveal();
	const rvUploadAlbums = reveal();
	const loading = $derived((library.loading || library.extrasLoading) && !all.length);
	const error = $derived(library.error ?? library.extrasError);
	// Only the empty states differ: signed out there is no account library to be missing yet.
	const signedOut = $derived(!auth.account?.signedIn);
	// What the sync button has left to push. Synced rows stay in the local library (they are what
	// the user still has after signing out), so counting all of `personal.saved` would nag forever.
	const toSync = $derived(unsynced(personal));

	onMount(load);

	// Only when the tab is opened: most accounts have no uploads at all, so this stays off the
	// Library's own load. `untrack` because the loader writes the very state it reads to decide
	// whether to run, which would otherwise re-trigger this effect forever.
	$effect(() => {
		if (tab === 'uploads') untrack(() => loadUploadAlbums());
	});

	function load() {
		loadLibrary(true);
		loadLibraryExtras(true);
	}

	let syncing = $state(false);
	async function sync() {
		if (syncing) return;
		syncing = true;
		const n = toSync.length;
		try {
			const { synced, failed } = await syncSavedToYouTube();
			if (failed && synced) toast(t('toasts.synced_partial', { synced, total: n, failed }));
			else if (failed) toast.error(t('toasts.synced_none', { failed }));
			else toast.success(t('toasts.synced_all', { count: synced }));
		} catch (e) {
			toast.error(String(e));
		} finally {
			syncing = false;
		}
	}

	// --- playlists tab: sort, view, last sync -----------------------------------------------------
	// Both persisted (`library_playlists_sort`, `library_playlists_view`); see plsort.ts for values.
	const SORT_OPTIONS = [
		{ value: 'default', label: 'library.playlists_sort_default' },
		{ value: 'title', label: 'library.playlists_sort_title' },
		{ value: 'title:desc', label: 'library.playlists_sort_title_desc' },
		{ value: 'count:desc', label: 'library.playlists_sort_count_desc' },
		{ value: 'count', label: 'library.playlists_sort_count' },
		{ value: 'synced:desc', label: 'library.playlists_sort_synced_desc' },
		{ value: 'synced', label: 'library.playlists_sort_synced' }
	] as const;
	let plSort = $state(formatSort(DEFAULT_SORT));
	let plView = $state<PlView>(DEFAULT_VIEW);
	const sortLabel = $derived(
		t((SORT_OPTIONS.find((o) => o.value === plSort) ?? SORT_OPTIONS[0]).label)
	);
	// Playlist id → its last complete sync. SQLite only, so re-read whenever a sync ends.
	let syncInfo = $state.raw<Record<string, PlaylistSyncInfo>>({});
	function loadSyncInfo() {
		api.playlistSyncInfo()
			.then((i) => (syncInfo = i))
			.catch(() => {});
	}
	// "2 h ago" ages while the page is open: a minute's tick is as fine as the line gets.
	let now = $state(Date.now() / 1000);
	onMount(() => {
		api.getSettings()
			.then((s) => {
				plSort = formatSort(parseSort(s.library_playlists_sort));
				plView = parseView(s.library_playlists_view);
			})
			.catch(() => {});
		loadSyncInfo();
		const off = api.onPlaylistIndexSynced(loadSyncInfo);
		const tick = setInterval(() => (now = Date.now() / 1000), 60_000);
		return () => {
			clearInterval(tick);
			void off.then((f) => f());
		};
	});
	const sortedPlaylists = $derived(sortPlaylists(playlists, parseSort(plSort), syncInfo));

	async function chooseSort(value: string) {
		const before = plSort;
		plSort = value;
		try {
			await api.setSetting('library_playlists_sort', value);
		} catch (e) {
			plSort = before;
			toast.error(String(e));
		}
	}
	async function chooseView(value: PlView) {
		if (value === plView) return;
		const before = plView;
		plView = value;
		try {
			await api.setSetting('library_playlists_view', value);
		} catch (e) {
			plView = before;
			toast.error(String(e));
		}
	}

	/** "Synced 2 h ago · +3 −1 ~2", or null for a playlist the monitor has not read yet. */
	function syncedText(id: string): string | null {
		const info = infoFor(syncInfo, id);
		if (!info) return null;
		const a = relativeAgo(info.synced_at, now);
		const ago = t(`library.sync_ago_${a.unit}`, { n: a.n });
		const changes = syncLine(info);
		return changes
			? t('library.synced_line_changes', { ago, changes })
			: t('library.synced_line', { ago });
	}

	// Sync all: the monitor's full run (`syncAllPlaylists`), its progress from `monitor.progress`
	// whoever started it. The scheduler holding the monitor answers `busy`.
	let startingSync = $state(false);
	const syncingAll = $derived(startingSync || monitor.progress !== null);
	async function syncAll() {
		if (syncingAll) return;
		startingSync = true;
		try {
			await syncAllPlaylists();
		} catch (e) {
			if (String(e) === 'busy') toast(t('monitor.busy'));
			else toast.error(String(e));
		} finally {
			startingSync = false;
			loadSyncInfo();
		}
	}
</script>

{#snippet grid(items: BrowseItem[], empty: string, rv: ReturnType<typeof reveal>, nudge = false)}
	{#if items.length}
		<div class="card-grid content-in">
			{#each items.slice(0, rv.count(items.length)) as item (item.kind + item.id)}
				<MediaCard {item} />
			{/each}
		</div>
		<!-- Outside the grid, or it would be laid out as a cell. -->
		{#if rv.more(items.length)}<div {@attach rv.sentinel}></div>{/if}
	{:else}
		<p class="text-sm text-muted-foreground">{empty}</p>
		<!-- An empty library is most likely a new one, and a new one is often someone coming from
		     Spotify (#375). Gone as soon as there is anything here, so it never needs dismissing. -->
		{#if nudge}
		<button
			class="mt-4 flex max-w-md cursor-pointer items-center gap-4 rounded-2xl border p-4 text-left transition-colors hover:bg-accent/10"
			onclick={() => openImport()}
		>
			<HugeiconsIcon icon={SpotifyIcon} class="h-8 w-8 shrink-0 text-primary" />
			<span>
				<span class="flex items-center gap-2 text-sm font-medium">
					{t('import.nudge_title')}
					<ExperimentalBadge />
				</span>
				<span class="block text-xs text-muted-foreground">{t('import.nudge_desc')}</span>
			</span>
		</button>
		{/if}
	{/if}
{/snippet}

<!-- Sort, view and Sync all, over the playlists tab only: the other tabs have neither a sync nor a
     count to sort by. -->
{#snippet playlistsToolbar()}
	<div class="mb-4 flex flex-wrap items-center gap-2">
		<Select.Root type="single" value={plSort} onValueChange={(v) => v && v !== plSort && chooseSort(v)}>
			<Select.Trigger size="sm" class="w-56" aria-label={t('library.playlists_sort')}>
				<HugeiconsIcon icon={Sorting01Icon} class="h-4 w-4 shrink-0" />
				<span class="flex-1 truncate text-left">{sortLabel}</span>
			</Select.Trigger>
			<Select.Content>
				{#each SORT_OPTIONS as o (o.value)}
					<Select.Item value={o.value} label={t(o.label)}>{t(o.label)}</Select.Item>
				{/each}
			</Select.Content>
		</Select.Root>
		<div class="flex items-center rounded-md border p-0.5" role="group">
			<Button
				variant={plView === 'grid' ? 'secondary' : 'ghost'}
				size="icon-sm"
				aria-label={t('library.view_grid')}
				aria-pressed={plView === 'grid'}
				title={t('library.view_grid')}
				onclick={() => chooseView('grid')}
			>
				<HugeiconsIcon icon={DashboardSquare02Icon} class="h-4 w-4" />
			</Button>
			<Button
				variant={plView === 'list' ? 'secondary' : 'ghost'}
				size="icon-sm"
				aria-label={t('library.view_list')}
				aria-pressed={plView === 'list'}
				title={t('library.view_list')}
				onclick={() => chooseView('list')}
			>
				<HugeiconsIcon icon={LeftToRightListBulletIcon} class="h-4 w-4" />
			</Button>
		</div>
		<div class="flex-1"></div>
		{#if !signedOut}
			<Button
				variant="outline"
				size="sm"
				class="gap-2"
				disabled={syncingAll}
				title={t('library.sync_all_tooltip')}
				onclick={syncAll}
			>
				<HugeiconsIcon icon={RefreshIcon} class="h-4 w-4 {syncingAll ? 'animate-spin' : ''}" />
				{#if monitor.progress}
					<span class="tabular-nums">
						{t('library.sync_progress', { done: monitor.progress.done, total: monitor.progress.total })}
					</span>
				{:else}
					{syncingAll ? t('library.syncing') : t('library.sync_all')}
				{/if}
			</Button>
		{/if}
	</div>
{/snippet}

<!-- The card grid with the last-sync line under each card. Rows a line taller than `.card-grid`'s
     pinned 12.75rem, for every card alike: an auto row would bring back the reflow that comment
     describes. -->
{#snippet playlistGrid(items: BrowseItem[], rv: ReturnType<typeof reveal>)}
	<div class="card-grid content-in" style="grid-auto-rows: 14rem">
		{#each items.slice(0, rv.count(items.length)) as item (item.kind + item.id)}
			{@const line = syncedText(item.id)}
			<div class="flex flex-col" style="contain-intrinsic-size: auto 14rem">
				<MediaCard {item} />
				{#if line}
					<span class="-mt-1.5 truncate px-2 text-[0.6875rem] tabular-nums text-muted-foreground" title={line}>
						{line}
					</span>
				{/if}
			</div>
		{/each}
	</div>
	{#if rv.more(items.length)}<div {@attach rv.sentinel}></div>{/if}
{/snippet}

<!-- One row per playlist: cover, title, subtitle, the last-sync line and the ⋯ menu. -->
{#snippet playlistRows(items: BrowseItem[], rv: ReturnType<typeof reveal>)}
	<div class="content-in flex flex-col">
		{#each items.slice(0, rv.count(items.length)) as item (item.kind + item.id)}
			{@const line = syncedText(item.id)}
			{@const cover = thumb(item.thumbnail, 96) ?? item.thumbnail}
			<div class="group/row relative flex items-center gap-2 rounded-lg pr-1 hover:bg-accent/10" data-ctx>
				<div
					class="flex min-w-0 flex-1 cursor-pointer items-center gap-3 px-2 py-1.5"
					role="button"
					tabindex="0"
					onclick={() => openItem(item)}
					onkeydown={(e) => {
						if (e.target !== e.currentTarget) return;
						if (e.key === 'Enter' || e.key === ' ') {
							e.preventDefault();
							openItem(item);
						}
					}}
				>
					<div class="flex h-12 w-12 shrink-0 items-center justify-center overflow-hidden rounded-md bg-muted text-muted-foreground/50">
						{#if cover}
							<img src={cover} alt="" class="h-full w-full object-cover" loading="lazy" draggable="false" />
						{:else}
							<HugeiconsIcon icon={Playlist02Icon} class="h-5 w-5" />
						{/if}
					</div>
					<div class="min-w-0 flex-1">
						<div class="truncate text-sm font-medium">{item.title}</div>
						{#if item.subtitle}
							<div class="truncate text-xs text-muted-foreground">{item.subtitle}</div>
						{/if}
					</div>
					{#if line}
						<span class="shrink-0 text-xs tabular-nums text-muted-foreground">{line}</span>
					{/if}
				</div>
				<ItemMenu
					{item}
					triggerClass="flex h-8 w-8 shrink-0 cursor-pointer items-center justify-center rounded-md text-muted-foreground opacity-0 transition hover:bg-accent/20 hover:text-foreground focus-visible:opacity-100 group-hover/row:opacity-100"
				/>
			</div>
		{/each}
	</div>
	{#if rv.more(items.length)}<div {@attach rv.sentinel}></div>{/if}
{/snippet}

<div class="p-6">
	<div class="mb-6 flex items-center justify-between">
		<h1 class="font-heading text-2xl font-bold">{t('library.title')}</h1>
		<div class="flex items-center gap-2">
			<!-- Only with something to push: saves made before signing in, which live on this
			     machine until this button puts them on the account. -->
			{#if auth.account?.signedIn && toSync.length}
				<!-- A cloud glyph with a number on it says nothing about what pressing it does, and
				     that's a write to someone's YouTube account. Hence a real tooltip rather than the
				     `title` this app uses elsewhere: it has to be read before the click, not after a
				     second of hovering. `child` keeps our own Button as the trigger element. -->
				<Tooltip.Provider delayDuration={150}>
					<Tooltip.Root>
						<Tooltip.Trigger>
							{#snippet child({ props })}
								<Button
									{...props}
									variant="outline"
									size="icon-sm"
									onclick={sync}
									disabled={syncing}
									aria-label={t('a11y.sync_to_ytm', { count: toSync.length })}
								>
									<span class="relative">
										<HugeiconsIcon
											icon={CloudSyncIcon}
											class="h-4 w-4 {syncing ? 'animate-pulse' : ''}"
										/>
										<!-- ring-background so the count reads over the icon's stroke (as in
										     Titlebar). -->
										<span
											class="absolute -right-2 -top-1.5 min-w-3.5 rounded-full bg-accent px-[3px] text-[9px] font-semibold leading-[0.875rem] text-accent-foreground ring-[1.5px] ring-background"
										>
											{toSync.length}
										</span>
									</span>
								</Button>
							{/snippet}
						</Tooltip.Trigger>
						<Tooltip.Content side="bottom">
							{syncing
								? t('common.loading')
								: t('library.sync_idle_tooltip', { count: toSync.length })}
						</Tooltip.Content>
					</Tooltip.Root>
				</Tooltip.Provider>
			{/if}
			<Button
				variant="outline"
				size="sm"
				class="gap-2"
				title={t('import.button_tooltip')}
				onclick={() => openImport()}
			>
				<HugeiconsIcon icon={SpotifyIcon} class="h-4 w-4" /> {t('import.button')}
				<ExperimentalBadge />
			</Button>
			<!-- Signed out too: a playlist can live on this machine with no account (#251). -->
			<Button variant="outline" size="sm" class="gap-2" onclick={() => openNewPlaylist()}>
				<HugeiconsIcon icon={Add01Icon} class="h-4 w-4" /> {t('nav.new_playlist')}
			</Button>
		</div>
	</div>


	<!-- The tabs always render: Local music needs neither an account nor a connection. -->
	<Tabs.Root bind:value={tab}>
		<Tabs.List class="mb-4">
			<Tabs.Trigger value="all">
				<HugeiconsIcon icon={SquareStackIcon} class="h-4 w-4" /> {t('common.all')}
			</Tabs.Trigger>
			<Tabs.Trigger value="playlists">
				<HugeiconsIcon icon={Playlist02Icon} class="h-4 w-4" /> {t('library.playlists_tab')}
			</Tabs.Trigger>
			<Tabs.Trigger value="albums">
				<HugeiconsIcon icon={MusicNoteSquare02Icon} class="h-4 w-4" /> {t('library.albums_tab')}
			</Tabs.Trigger>
			<Tabs.Trigger value="artists">
				<HugeiconsIcon icon={UserSharingIcon} class="h-4 w-4" /> {t('library.artists_tab')}
			</Tabs.Trigger>
			<Tabs.Trigger value="songs">
				<HugeiconsIcon icon={MusicNote01Icon} class="h-4 w-4" /> {t('library.songs_tab')}
			</Tabs.Trigger>
			<Tabs.Trigger value="uploads">
				<HugeiconsIcon icon={CloudUploadIcon} class="h-4 w-4" /> {t('library.uploads_tab')}
			</Tabs.Trigger>
			<Tabs.Trigger value="local">
				<HugeiconsIcon icon={DriveIcon} class="h-4 w-4" /> {t('library.local_tab')}
			</Tabs.Trigger>
			<Tabs.Trigger value="everywhere">
				<HugeiconsIcon icon={LeftToRightListBulletIcon} class="h-4 w-4" /> {t('everywhere.tab')}
				{#if alertCount}
					<span class="ml-1 rounded-full bg-destructive px-1.5 text-[10px] font-semibold text-white tabular-nums">
						{alertCount > 99 ? '99+' : alertCount}
					</span>
				{/if}
			</Tabs.Trigger>
		</Tabs.List>
		<!-- Every branch below is gated on `tab`, because bits-ui never unmounts an inactive panel: it
		     renders every one and hides the inactive ones. Left alone, opening Library builds each card twice
		     (once for All, once for its own tab) and mounts the whole Local tab, disk scan included,
		     for a panel you cannot see. -->
		<!-- Songs, Uploads and Local stand apart: two are track lists rather than card grids, the
		     third needs neither an account nor a connection, and the states below fit none of them. -->
		<Tabs.Content value="everywhere">
			{#if tab === 'everywhere'}
				<PlaylistSongs onalerts={(n) => (alertCount = n)} />
			{/if}
		</Tabs.Content>
		<Tabs.Content value="songs">
			{#if tab === 'songs'}
				{#if signedOut}
					<p class="text-sm text-muted-foreground">{t('library.songs_signed_out')}</p>
				{:else}
					<LibrarySongs />
				{/if}
			{/if}
		</Tabs.Content>
		<Tabs.Content value="uploads">
			{#if tab === 'uploads'}
				{#if signedOut}
					<p class="text-sm text-muted-foreground">{t('library.uploads_signed_out')}</p>
				{:else}
					<!-- `line` rather than the pill row above it: two identical pill rows stacked read as
					     one control drawn twice. -->
					<Tabs.Root bind:value={uploadTab}>
						<Tabs.List variant="line" class="mb-4">
							<Tabs.Trigger value="all">
								<HugeiconsIcon icon={SquareStackIcon} class="h-4 w-4" /> {t('common.all')}
							</Tabs.Trigger>
							<Tabs.Trigger value="songs">
								<HugeiconsIcon icon={MusicNote01Icon} class="h-4 w-4" /> {t('common.songs')}
							</Tabs.Trigger>
							<Tabs.Trigger value="albums">
								<HugeiconsIcon icon={MusicNoteSquare02Icon} class="h-4 w-4" /> {t('common.albums')}
							</Tabs.Trigger>
						</Tabs.List>
						<Tabs.Content value="all">
							{#if uploadTab === 'all'}
								<LibrarySongs uploads limit={20} onSeeAll={() => (uploadTab = 'songs')} />
								{#if library.uploadAlbums.length}
									<h2 class="mb-3 mt-8 font-heading text-lg font-semibold">{t('common.albums')}</h2>
									{@render grid(library.uploadAlbums, '', rvUploadsAll)}
								{/if}
							{/if}
						</Tabs.Content>
						<Tabs.Content value="songs">
							{#if uploadTab === 'songs'}<LibrarySongs uploads />{/if}
						</Tabs.Content>
						<Tabs.Content value="albums">
							{#if uploadTab === 'albums'}
								{#if library.uploadAlbumsLoading && !library.uploadAlbums.length}
									<div class="card-grid">
										{#each Array(6) as _, i (i)}
											<MediaCardSkeleton />
										{/each}
									</div>
								{:else if library.uploadAlbumsError && !library.uploadAlbums.length}
									<ErrorState
										message={library.uploadAlbumsError}
										onRetry={() => loadUploadAlbums(true)}
									/>
								{:else}
									{@render grid(
										library.uploadAlbums,
										t('library.no_upload_albums'),
										rvUploadAlbums
									)}
								{/if}
							{/if}
						</Tabs.Content>
					</Tabs.Root>
				{/if}
			{/if}
		</Tabs.Content>
		<Tabs.Content value="local">{#if tab === 'local'}<LocalMusic />{/if}</Tabs.Content>
		{#if tab === 'local' || tab === 'songs' || tab === 'uploads'}
			<!-- nothing else: the grid states below have no bearing on these three -->
		{:else if loading}
			<div class="card-grid">
				{#each Array(12) as _, i (i)}
					<MediaCardSkeleton />
				{/each}
			</div>
		{:else if error && !all.length}
			<!-- Only when there is nothing to fall back on. Now that the grid is cached across visits, a
			     refresh that fails should leave the library you were looking at on screen. -->
			<ErrorState message={error} onRetry={load} />
		{:else}
			<Tabs.Content value="all">
				{#if tab === 'all'}
					{@render grid(
						all,
						signedOut ? t('library.empty_signed_out') : t('library.empty'),
						rvAll,
						true
					)}
				{/if}
			</Tabs.Content>
			<Tabs.Content value="playlists">
				{#if tab === 'playlists'}
					{#if playlists.length}
						{@render playlistsToolbar()}
					{/if}
					{#if plView === 'list' && playlists.length}
						{@render playlistRows(sortedPlaylists, rvPlaylists)}
					{:else if playlists.length}
						{@render playlistGrid(sortedPlaylists, rvPlaylists)}
					{:else}
						{@render grid(playlists, t('library.no_saved_playlists'), rvPlaylists, true)}
					{/if}
				{/if}
			</Tabs.Content>
			<Tabs.Content value="albums">
				{#if tab === 'albums'}
					{@render grid(
						albums,
						t('library.no_saved_albums'),
						rvAlbums
					)}
				{/if}
			</Tabs.Content>
			<Tabs.Content value="artists">
				{#if tab === 'artists' && signedOut}
					{@render grid(artists, t('library.no_saved_artists'), rvArtists)}
				{:else if tab === 'artists'}
					<!-- `line`, same as Uploads: a second pill row would read as the first drawn twice. -->
					<Tabs.Root bind:value={artistTab}>
						<Tabs.List variant="line" class="mb-4">
							<Tabs.Trigger value="artists">
								<HugeiconsIcon icon={UserSharingIcon} class="h-4 w-4" /> {t('library.artists_tab')}
							</Tabs.Trigger>
							<Tabs.Trigger value="subscriptions">
								<HugeiconsIcon icon={UserCheck01Icon} class="h-4 w-4" /> {t('library.subscriptions_tab')}
							</Tabs.Trigger>
						</Tabs.List>
						<Tabs.Content value="artists">
							{#if artistTab === 'artists'}
								<!-- Unmerged: what was saved here is a subscription, and lists under that. -->
								{@render grid(library.artists, t('library.no_artists'), rvArtists)}
							{/if}
						</Tabs.Content>
						<Tabs.Content value="subscriptions">
							{#if artistTab === 'subscriptions'}
								{@render grid(subscriptions, t('library.no_subscriptions'), rvSubscriptions)}
							{/if}
						</Tabs.Content>
					</Tabs.Root>
				{/if}
			</Tabs.Content>
		{/if}
	</Tabs.Root>
</div>
