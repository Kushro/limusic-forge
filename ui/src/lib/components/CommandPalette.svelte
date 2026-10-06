<script lang="ts">
	// Ctrl+K search, without leaving the page you're on. Runs the same debounced suggestions request
	// the search field runs (SearchSuggest), so the two show the same rows. The matching rows come
	// before the query completions because bits-ui highlights the first row after every update, and
	// that keeps Enter on the best match rather than on a completion of what you typed.
	//
	// shouldFilter={false}: the rows come back already ranked by YouTube, and re-scoring them against
	// the raw query locally would hide results whose title doesn't contain what you typed.
	// vimBindings={false}: those bind ctrl+k to "move up", which is the key that opens this.
	//
	// Above the YouTube rows sit the palette's own commands (`palette.ts`): places to go, your
	// playlists, and app actions. Those are ranked here, locally, because they are ours and YouTube
	// knows nothing about them. With an empty field they are all the palette shows; with a query
	// they stay on top, so Enter on "settings" opens Settings instead of a song called that.
	import { goto } from '$app/navigation';
	import { toggleMode } from 'mode-watcher';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		Search01Icon,
		HistoryIcon,
		MusicNote01Icon,
		UserIcon,
		Home01Icon,
		LibraryIcon,
		Notification03Icon,
		Playlist02Icon,
		Settings01Icon,
		DatabaseImportIcon,
		Sun01Icon,
		Radar01Icon,
		RefreshIcon
	} from '@hugeicons/core-free-icons';
	import * as Command from '$lib/components/ui/command/index.js';
	import { Skeleton } from '$lib/components/ui/skeleton';
	import ExplicitIcon from './ExplicitIcon.svelte';
	import ItemMenu from './ItemMenu.svelte';
	import { searchSuggestions, type BrowseItem, type SearchSuggestions } from '$lib/api';
	import { openItem, rowMeta } from '$lib/browse';
	import { auth, library, personal, syncAllPlaylists, toast, ui } from '$lib/player.svelte';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { rankCommands, type PaletteCommand } from '$lib/palette';
	import { thumb } from '$lib/thumb';
	import { t, type TranslationKey } from '$lib/i18n.svelte';

	let query = $state('');
	let items = $state<BrowseItem[]>([]);
	let queries = $state<SearchSuggestions['queries']>([]);
	let loading = $state(false);
	let loadedFor = ''; // query `items` belongs to, so a stale response can't land
	// The row a right-click menu belongs to: whatever the pointer last entered. One menu for the
	// whole dialog, because `data-ctx` sits on the dialog itself (see below) and only one row can be
	// under the pointer.
	let ctxItem = $state<BrowseItem | null>(null);

	// The menu's popup lives on <body>, which the dialog counts as an interaction outside itself and
	// would close on, unmounting the menu mid-click. `data-menu` marks the popup and its backdrop, so
	// clicking one is treated as still being inside. Everything else outside still dismisses.
	const inMenu = (e: Event) => {
		const t = e.target;
		return t instanceof Element && !!t.closest('[data-menu]');
	};

	// Opening is itself a keystroke, so nothing is fetched until the typing pauses. `loading` is set
	// on the keystroke rather than when the timer fires: otherwise the empty list reads as "no
	// results" for the whole debounce, on every query.
	$effect(() => {
		const q = query.trim();
		if (q.length < 2) {
			[items, queries] = [[], []];
			loading = false;
			loadedFor = '';
			return;
		}
		if (q === loadedFor) return;
		[items, queries] = [[], []];
		loading = true;
		const timer = setTimeout(() => load(q), 300);
		return () => clearTimeout(timer);
	});

	// Closing clears the field, which the effect above turns into an empty list: reopening starts
	// fresh instead of on the last search's rows.
	$effect(() => {
		if (!ui.paletteOpen) query = '';
	});

	// A modal opened off a row (right-click ▸ Add to playlist, Share) is a plain fixed layer in the
	// layout at z-50, while this dialog portals to <body>: the z ties and DOM order decides, so the
	// modal opens behind the palette. Raising its z wouldn't be enough either, since the dialog's
	// focus trap pulls focus straight back out of the modal's filter field. So the palette steps
	// aside rather than fighting for the layer. Issue #218.
	$effect(() => {
		if (ui.addSongs || ui.share) ui.paletteOpen = false;
	});

	async function load(q: string) {
		loadedFor = q;
		try {
			const next = await searchSuggestions(q);
			if (loadedFor === q) ({ items, queries } = next);
		} catch {
			if (loadedFor === q) [items, queries] = [[], []];
		} finally {
			if (loadedFor === q) loading = false;
		}
	}

	function choose(item: BrowseItem) {
		ui.paletteOpen = false;
		openItem(item); // a song plays, everything else opens its page
	}

	function search(q: string) {
		if (!q) return;
		ui.paletteOpen = false;
		goto(`/search?q=${encodeURIComponent(q)}`);
	}

	// --- Commands -------------------------------------------------------------------------------
	type Icon = typeof Home01Icon;

	/** Every command closes the palette first, so whatever it opens isn't behind it. */
	const close = (run: () => void) => () => {
		ui.paletteOpen = false;
		run();
	};
	const go = (path: string) => close(() => goto(path));

	// The sidebar's progress and the Monitor page show the run; this only reports how it ended.
	function checkPlaylists() {
		syncAllPlaylists()
			.then(() => toast.success(t('monitor.check_done')))
			.catch((e) => (String(e) === 'busy' ? toast(t('monitor.busy')) : toast.error(String(e))));
	}

	function openSettings(tab?: 'import') {
		ui.settingsFocus = tab ? { tab } : null;
		ui.settingsOpen = true;
	}

	// The Library page's tabs, in its own order (`routes/library/+page.svelte`); All is the page
	// itself, already listed as Library.
	const LIBRARY_TABS: [string, TranslationKey][] = [
		['playlists', 'library.playlists_tab'],
		['albums', 'library.albums_tab'],
		['artists', 'library.artists_tab'],
		['songs', 'library.songs_tab'],
		['uploads', 'library.uploads_tab'],
		['local', 'library.local_tab'],
		['everywhere', 'everywhere.tab']
	];

	// Keywords are hidden aliases, not shown, so they stay English: they only ever add matches.
	// $derived so the labels follow a language change.
	const gotoCmds: PaletteCommand[] = $derived([
		{ id: 'goto:home', group: 'goto', label: t('nav.home'), run: go('/') },
		{ id: 'goto:search', group: 'goto', label: t('nav.search'), run: go('/search') },
		{ id: 'goto:library', group: 'goto', label: t('nav.library'), run: go('/library') },
		{
			id: 'goto:alerts',
			group: 'goto',
			label: t('nav.alerts'),
			keywords: ['monitor', 'changes', 'notifications', 'timeline'],
			run: go('/alerts')
		},
		{
			id: 'goto:monitor',
			group: 'goto',
			label: t('nav.monitor'),
			keywords: ['sync', 'check', 'runs', 'stats', 'backups', 'interval'],
			run: go('/monitor')
		},
		...LIBRARY_TABS.map(
			([tab, key]): PaletteCommand => ({
				id: `goto:library:${tab}`,
				group: 'goto',
				label: t('palette.library_tab', { tab: t(key) }),
				keywords: ['library'],
				run: go(`/library?tab=${tab}`)
			})
		)
	]);

	const actionCmds: PaletteCommand[] = $derived([
		{
			id: 'action:settings',
			group: 'actions',
			label: t('nav.settings'),
			keywords: ['preferences', 'options', 'config'],
			run: close(() => openSettings())
		},
		{
			id: 'action:import',
			group: 'actions',
			label: t('settings.tabs.import'),
			keywords: ['migrate', 'import', 'playlistforge', 'limusic', 'settings'],
			run: close(() => openSettings('import'))
		},
		// Signed out there is nothing of yours to check, so the action isn't offered.
		...(auth.account?.signedIn
			? [
					{
						id: 'action:check-playlists',
						group: 'actions',
						label: t('palette.check_playlists'),
						keywords: ['sync', 'monitor', 'refresh', 'alerts', 'scan'],
						run: close(checkPlaylists)
					} satisfies PaletteCommand
				]
			: []),
		{
			id: 'action:theme',
			group: 'actions',
			label: t('a11y.toggle_theme'),
			keywords: ['dark', 'light', 'mode', 'theme'],
			run: close(toggleMode)
		}
	]);

	// Same list, same order as the sidebar's: what this machine saved, then the account's library,
	// pins first.
	const playlistItems = $derived(orderLibrary(mergeSaved(personal, library.items, 'playlist'), personal));
	const playlistById = $derived(new Map(playlistItems.map((i) => [`pl:${i.id}`, i])));
	const playlistCmds: PaletteCommand[] = $derived(
		playlistItems.map((item) => ({
			id: `pl:${item.id}`,
			group: 'playlists',
			label: item.title,
			run: close(() => openItem(item))
		}))
	);

	const ICONS: Record<string, Icon> = {
		'goto:home': Home01Icon,
		'goto:search': Search01Icon,
		'goto:alerts': Notification03Icon,
		'goto:monitor': Radar01Icon,
		'action:check-playlists': RefreshIcon,
		'action:settings': Settings01Icon,
		'action:import': DatabaseImportIcon,
		'action:theme': Sun01Icon
	};
	const iconFor = (cmd: PaletteCommand): Icon =>
		ICONS[cmd.id] ?? (cmd.group === 'playlists' ? Playlist02Icon : LibraryIcon);

	// Fewer rows with nothing typed, so the empty palette is a short menu rather than a wall; a
	// query widens each group since it is already narrowing them.
	const typed = $derived(query.trim().length > 0);
	const commandGroups = $derived(
		[
			{ heading: t('palette.go_to'), cmds: rankCommands(query, gotoCmds, typed ? 5 : 3) },
			{ heading: t('common.playlists'), cmds: rankCommands(query, playlistCmds, typed ? 6 : 5) },
			{ heading: t('palette.actions'), cmds: rankCommands(query, actionCmds, 4) }
		].filter((g) => g.cmds.length)
	);
</script>

<Command.Dialog
	bind:open={ui.paletteOpen}
	shouldFilter={false}
	vimBindings={false}
	loop
	title={t('common.search')}
	description={t('common.command_description')}
	class="sm:max-w-xl"
	contentProps={{
		// data-ctx: right-clicking a row opens that item's menu at the pointer (see `ctxHost`). The
		// input keeps WebKit's own menu (`wantsNative`).
		'data-ctx': '',
		// Closing hands focus back to whatever held it before Ctrl+K, which would take it off the
		// modal that just replaced the palette (see the effect above).
		onCloseAutoFocus: (e: Event) => {
			// Settings too: a palette command (or Ctrl+P over the palette) just opened it.
			if (ui.addSongs || ui.share || ui.settingsOpen) e.preventDefault();
		},
		onInteractOutside: (e: PointerEvent) => {
			if (inMenu(e)) e.preventDefault();
		},
		onFocusOutside: (e: FocusEvent) => {
			if (inMenu(e)) e.preventDefault();
		}
	}}
>
	<Command.Input bind:value={query} placeholder={t('common.search_placeholder')} />
	<Command.List class="max-h-[22rem]">
		{#each commandGroups as group (group.heading)}
			<Command.Group heading={group.heading}>
				{#each group.cmds as cmd (cmd.id)}
					{@const item = playlistById.get(cmd.id)}
					<!-- A playlist row keeps its right-click menu; everything else has none. -->
					<Command.Item
						value={`cmd:${cmd.id}`}
						onSelect={cmd.run}
						onmouseenter={() => (ctxItem = item ?? null)}
						class="gap-3 px-2 py-1.5"
					>
						{#if item?.thumbnail}
							<img
								src={thumb(item.thumbnail, 400)}
								alt=""
								class="h-6 w-6 shrink-0 rounded object-cover"
							/>
						{:else}
							<div class="flex h-6 w-6 shrink-0 items-center justify-center text-muted-foreground">
								<HugeiconsIcon icon={iconFor(cmd)} class="h-4 w-4" />
							</div>
						{/if}
						<span class="truncate text-sm">{cmd.label}</span>
					</Command.Item>
				{/each}
			</Command.Group>
		{/each}
		{#if loading}
			{#each Array(4) as _, i (i)}
				<div class="flex items-center gap-3 px-3 py-2">
					<Skeleton class="h-10 w-10 shrink-0 rounded-md" />
					<div class="min-w-0 flex-1">
						<Skeleton class="h-3 w-40 rounded" />
						<Skeleton class="mt-2 h-2.5 w-24 rounded" />
					</div>
				</div>
			{/each}
		{:else if !items.length && !queries.length}
			<!-- Only when the commands found nothing either: under a list of them it would read as
			     "none of these". -->
			{#if !commandGroups.length}
				<div class="px-4 py-6 text-center text-sm text-muted-foreground">
					{query.trim().length < 2 ? t('common.type_to_search') : t('common.nothing_quick')}
				</div>
			{/if}
		{:else}
			{#if items.length}
				<Command.Group heading={t('common.results')}>
					{#each items as item (item.id)}
						<Command.Item
							value={item.id}
							onSelect={() => choose(item)}
							onmouseenter={() => (ctxItem = item)}
							class="gap-3 px-2 py-1.5"
						>
							{#if item.thumbnail}
								<!-- 400, the same size the cards ask for: the CDN doesn't serve every rewritten
								     size, that one is verified, and the row lands on an image the grid already
								     fetched. -->
								<img
									src={thumb(item.thumbnail, 400)}
									alt=""
									class="h-10 w-10 shrink-0 object-cover {item.kind === 'artist'
										? 'rounded-full'
										: 'rounded-md'}"
								/>
							{:else}
								<div
									class="flex h-10 w-10 shrink-0 items-center justify-center bg-muted text-muted-foreground/50 {item.kind ===
									'artist'
										? 'rounded-full'
										: 'rounded-md'}"
								>
									<HugeiconsIcon
										icon={item.kind === 'artist' ? UserIcon : MusicNote01Icon}
										class="h-5 w-5"
									/>
								</div>
							{/if}
							<div class="min-w-0 flex-1">
								<div class="truncate text-sm">{item.title}</div>
								<div class="flex items-center gap-1 text-xs text-muted-foreground">
									{#if item.explicit}
										<ExplicitIcon class="h-3 w-3 shrink-0" />
									{/if}
									<span class="truncate">{rowMeta(item)}</span>
								</div>
							</div>
						</Command.Item>
					{/each}
				</Command.Group>
			{/if}
			{#if queries.length}
				<Command.Group>
					{#each queries as q (q.text)}
						<!-- Clears the right-click target: a completion has no item menu, and leaving the last
						     hovered row's would open it over a row it isn't about. -->
						<Command.Item
							value={`q:${q.text}`}
							onSelect={() => search(q.text)}
							onmouseenter={() => (ctxItem = null)}
							class="gap-3 px-2 py-2"
						>
							<!-- altIcon/showAlt, not a ternary: HugeiconsIcon freezes `icon` at mount. -->
							<HugeiconsIcon
								icon={Search01Icon}
								altIcon={HistoryIcon}
								showAlt={q.history}
								class="h-4 w-4 shrink-0 text-muted-foreground"
							/>
							<span class="truncate text-sm">{q.text}</span>
						</Command.Item>
					{/each}
				</Command.Group>
			{/if}
		{/if}

		{#if query.trim().length >= 2}
			<Command.Group>
				<Command.Item
					value="__all__"
					onSelect={() => search(query.trim())}
					class="gap-2 text-muted-foreground"
				>
					<HugeiconsIcon icon={Search01Icon} class="h-3.5 w-3.5" />
					<span class="truncate">{t('common.all_results_for', { query: query.trim() })}</span>
				</Command.Item>
			</Command.Group>
		{/if}
	</Command.List>
	<!-- No visible trigger: a palette row is too small for a hover-only ⋯, and this only ever opens
	     from a right-click. -->
	{#if ctxItem}
		<ItemMenu item={ctxItem} triggerClass="hidden" />
	{/if}
</Command.Dialog>
