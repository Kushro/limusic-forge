<script lang="ts">
	// The monitor's alerts, all of them (PlaylistForge's Alerts screen): every change a sync found in
	// one of your playlists, newest first in day buckets, filtered by kind, playlist and seen. Each row
	// plays the song, opens it on YouTube Music, or goes to its playlist. Opening the page marks
	// nothing seen (PlaylistForge doesn't either): that is "Mark all as seen", or a row's own check.
	//
	// The Timeline tab is PlaylistForge's per-playlist history: every snapshot kept of one playlist
	// with the changes since the one before, expandable.
	//
	// `?playlist=<id>` opens filtered to one playlist (Library ▸ In your playlists links here so),
	// and `?tab=timeline` on its history.
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		ArrowDown01Icon,
		ArrowUp01Icon,
		HistoryIcon,
		LinkSquare02Icon,
		MusicNote01Icon,
		Notification03Icon,
		PlayIcon,
		Playlist02Icon,
		Tick02Icon,
		TickDouble02Icon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { AlertKind, PlaylistAlert, SongItem, TimelineEntry } from '$lib/api';
	import { ON_REPEAT_ID, isLocalPlaylist } from '$lib/api';
	import {
		ALERT_KINDS,
		alertPlaylists,
		countByKind,
		daysAgo,
		filterAlerts,
		groupByDay,
		localDay,
		timelineSums,
		unseenIds,
		ytmSongUrl
	} from '$lib/alerts';
	import { auth, library, personal, playSong, toast } from '$lib/player.svelte';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { thumb } from '$lib/thumb';
	import { t } from '$lib/i18n.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Switch } from '$lib/components/ui/switch';
	import PlaylistSelect from '$lib/components/PlaylistSelect.svelte';
	import * as Tabs from '$lib/components/ui/tabs';
	import ErrorState from '$lib/components/ErrorState.svelte';

	// The "every playlist" item: the select's value can't be empty, so it stands for `playlist = ''`.
	const ALL = '*';

	let tab = $state(page.url.searchParams.get('tab') === 'timeline' ? 'timeline' : 'alerts');
	let alerts = $state.raw<PlaylistAlert[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);

	let kinds = $state<AlertKind[]>([]);
	let playlist = $state(page.url.searchParams.get('playlist') ?? '');
	let unseenOnly = $state(false);

	// Alerts are never deleted, so the list loads newest first in pages of PAGE rows, the next one
	// when the end of the list comes into view. Filters and counts work on what is loaded.
	const PAGE = 500;
	// The last page came back full: there may be older rows still to load.
	let more = $state(false);
	let loadingMore = $state(false);
	// Bumped by every reload, so a page that was in flight across one is dropped.
	let gen = 0;

	// From the newest again (a sync filed more, or rows were marked seen), as deep as the list
	// already went, so a reload never pulls rows out from under where the user is.
	async function load() {
		const g = ++gen;
		const limit = Math.max(PAGE, alerts.length);
		try {
			const rows = await api.playlistAlerts({ all: true, limit });
			if (g !== gen) return;
			alerts = rows;
			more = rows.length === limit;
			error = null;
		} catch (e) {
			if (g === gen) error = String(e);
		} finally {
			if (g === gen) loading = false;
		}
	}
	async function loadMore() {
		const last = alerts.at(-1);
		if (!more || loadingMore || !last || last.id === undefined) return;
		const g = gen;
		loadingMore = true;
		try {
			const before: [number, number] = [last.at, last.id];
			const rows = await api.playlistAlerts({ all: true, limit: PAGE, before });
			if (g !== gen) return;
			alerts = [...alerts, ...rows];
			more = rows.length === PAGE;
		} catch (e) {
			if (g === gen) toast.error(String(e));
		} finally {
			loadingMore = false;
		}
	}
	// Loads the next page while the end of the list is in view (or near it). It reads
	// `alerts.length`, so each page re-creates the observer, whose first callback loads another
	// one if the end is still in view (filters that leave little of a page shown).
	function endOfList(el: HTMLElement) {
		void alerts.length;
		const io = new IntersectionObserver(
			(entries) => {
				if (entries.some((e) => e.isIntersecting)) loadMore();
			},
			{ rootMargin: '600px 0px' }
		);
		io.observe(el);
		return () => io.disconnect();
	}
	onMount(() => {
		load();
		// A sync filed new ones, or something (here or elsewhere) marked them seen.
		const offs = [api.onAlertsChanged(() => load()), api.onPlaylistIndexSynced(() => load())];
		return () => offs.forEach((off) => void off.then((f) => f()));
	});

	// Names from the library, as the sidebar lists it; a playlist no longer there falls back to the
	// title its newest snapshot was taken under, then to the bare word.
	const playlists = $derived(orderLibrary(mergeSaved(personal, library.items, 'playlist'), personal));
	const names = $derived(new Map(playlists.map((p) => [p.id, p.title])));
	let snapTitles = $state<Record<string, string>>({});
	const nameOf = (id: string) => names.get(id) ?? snapTitles[id] ?? t('common.playlist_singular');
	const playlistHref = (id: string) => `/playlist/${encodeURIComponent(id)}`;

	// --- alerts -----------------------------------------------------------------------------------
	const byPlaylist = $derived(playlist ? filterAlerts(alerts, { playlist }) : alerts);
	const counts = $derived(countByKind(byPlaylist));
	const shown = $derived(filterAlerts(alerts, { kinds, playlist: playlist || null, unseenOnly }));
	const days = $derived(groupByDay(shown));
	const today = $derived(localDay(Date.now() / 1000));
	const unseen = $derived(unseenIds(alerts));
	const filtered = $derived(kinds.length > 0 || !!playlist || unseenOnly);
	const alertPlaylistIds = $derived(alertPlaylists(alerts).map((p) => p.id));

	function toggleKind(k: AlertKind) {
		kinds = kinds.includes(k) ? kinds.filter((x) => x !== k) : [...kinds, k];
	}
	function clearFilters() {
		kinds = [];
		playlist = '';
		unseenOnly = false;
	}

	function dayLabel(day: string) {
		const n = daysAgo(day, today);
		if (n === 0) return t('alerts.today');
		if (n === 1) return t('alerts.yesterday');
		const [y, m, d] = day.split('-').map(Number);
		return new Date(y, m - 1, d).toLocaleDateString(undefined, { dateStyle: 'full' });
	}
	const timeOf = (at: number) =>
		new Date(at * 1000).toLocaleTimeString(undefined, { timeStyle: 'short' });

	async function markSeen(ids?: number[]) {
		const before = alerts;
		const marked = new Set(ids ?? unseen);
		// Straight away here; the `alerts-changed` that follows re-reads the list and the badge.
		alerts = alerts.map((a) => (a.id !== undefined && marked.has(a.id) ? { ...a, seen: true } : a));
		try {
			await api.markAlertsSeen(ids);
		} catch (e) {
			alerts = before;
			toast.error(String(e));
		}
	}

	// A song can be played unless it went unavailable, which is the one thing YouTube says it can't.
	const playable = (a: { kind: AlertKind; song: SongItem | null }) =>
		!!a.song && a.kind !== 'unavailable' && !a.song.unavailable;
	const play = (s: SongItem) => playSong(s).catch((e) => toast.error(String(e)));
	const openYtm = (videoId: string) =>
		api.openExternal(ytmSongUrl(videoId)).catch((e) => toast.error(String(e)));

	function openTimeline(id: string) {
		timelineFor = id;
		tab = 'timeline';
	}

	// --- timeline ---------------------------------------------------------------------------------
	// The playlists that can have one: yours, from YouTube (a playlist on this machine, or On Repeat,
	// is never synced), with the ones alerts were filed for first.
	const timelineOptions = $derived.by(() => {
		const ids = new Set(alertPlaylistIds);
		for (const p of playlists)
			if (p.kind === 'playlist' && p.id !== ON_REPEAT_ID && !isLocalPlaylist(p.id)) ids.add(p.id);
		return [...ids];
	});
	let timelineFor = $state(page.url.searchParams.get('playlist') ?? '');
	let timeline = $state.raw<TimelineEntry[]>([]);
	let timelineLoading = $state(false);
	let open = $state<Set<number>>(new Set());

	// Without a pick, the playlist with the most alerts: the one most likely asked about.
	$effect(() => {
		if (!timelineFor && alertPlaylistIds.length) timelineFor = alertPlaylistIds[0];
	});

	$effect(() => {
		const id = timelineFor;
		if (!id || tab !== 'timeline') return;
		let stale = false;
		timelineLoading = true;
		api
			.playlistTimeline(id)
			.then((entries) => {
				if (stale) return;
				timeline = entries;
				open = new Set(entries.length && !entries[0].baseline ? [entries[0].snapshot_id] : []);
				const title = entries.find((e) => e.title)?.title;
				if (title && !snapTitles[id]) snapTitles = { ...snapTitles, [id]: title };
			})
			.catch((e) => !stale && toast.error(String(e)))
			.finally(() => !stale && (timelineLoading = false));
		return () => (stale = true);
	});

	function toggleEntry(id: number) {
		const next = new Set(open);
		if (next.has(id)) next.delete(id);
		else next.add(id);
		open = next;
	}
	const whenOf = (at: number) =>
		new Date(at * 1000).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });

	const kindLabel = (k: AlertKind) => t(`everywhere.kind_${k}`);
	const kindVariant = (k: AlertKind) =>
		k === 'removed' || k === 'unavailable' ? ('muted' as const) : ('label' as const);
</script>

{#snippet songCell(song: SongItem | null, videoId: string)}
	{#if song?.thumbnail}
		<img src={thumb(song.thumbnail, 96)} alt="" loading="lazy" class="h-10 w-10 shrink-0 rounded-md object-cover" />
	{:else}
		<span class="flex h-10 w-10 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground/50">
			<HugeiconsIcon icon={MusicNote01Icon} class="h-4 w-4" />
		</span>
	{/if}
	<span class="min-w-0 flex-1">
		<span class="block truncate text-sm font-medium">{song?.title ?? videoId}</span>
		{#if song?.artists}
			<span class="block truncate text-xs text-muted-foreground">{song.artists}</span>
		{/if}
	</span>
{/snippet}

{#snippet songActions(song: SongItem | null, kind: AlertKind, videoId: string)}
	{#if song && playable({ kind, song })}
		<Button variant="ghost" size="icon-sm" onclick={() => play(song)} title={t('alerts.play')} aria-label={t('alerts.play')}>
			<HugeiconsIcon icon={PlayIcon} class="h-4 w-4" />
		</Button>
	{/if}
	<Button variant="ghost" size="icon-sm" onclick={() => openYtm(videoId)} title={t('alerts.open_ytm')} aria-label={t('alerts.open_ytm')}>
		<HugeiconsIcon icon={LinkSquare02Icon} class="h-4 w-4" />
	</Button>
{/snippet}

<div class="p-6">
	<div class="mb-4 flex flex-wrap items-start gap-3">
		<div class="min-w-0 flex-1">
			<h1 class="flex items-center gap-2 font-heading text-2xl font-bold tracking-tight">
				<HugeiconsIcon icon={Notification03Icon} class="h-6 w-6 text-primary" />
				{t('alerts.title')}
				{#if unseen.length}
					<Badge variant="chip">{t('alerts.unseen_count', { count: unseen.length })}</Badge>
				{/if}
			</h1>
			<p class="mt-1 text-sm text-muted-foreground">
				{tab === 'timeline' ? t('alerts.timeline_intro') : t('alerts.intro')}
			</p>
		</div>
		{#if tab === 'alerts'}
			<Button variant="outline" size="sm" class="gap-1.5" disabled={!unseen.length} onclick={() => markSeen()}>
				<HugeiconsIcon icon={TickDouble02Icon} class="h-4 w-4" />
				{t('alerts.mark_all_seen')}
			</Button>
		{/if}
	</div>

	<Tabs.Root bind:value={tab}>
		<Tabs.List class="mb-4">
			<Tabs.Trigger value="alerts">{t('alerts.tab_alerts')}</Tabs.Trigger>
			<Tabs.Trigger value="timeline">{t('alerts.tab_timeline')}</Tabs.Trigger>
		</Tabs.List>

		<Tabs.Content value="alerts">
			{#if loading}
				{#each Array(6) as _, i (i)}
					<div class="mb-2 h-14 animate-pulse rounded-lg bg-card/40"></div>
				{/each}
			{:else if error}
				<ErrorState message={error} onRetry={load} />
			{:else if !alerts.length}
				<p class="text-sm text-muted-foreground">
					{auth.account?.signedIn ? t('alerts.empty') : t('alerts.empty_signed_out')}
				</p>
			{:else}
				<div class="mb-4 flex flex-wrap items-center gap-2" role="group" aria-label={t('alerts.filter_kind')}>
					{#each ALERT_KINDS as k (k)}
						<button
							type="button"
							aria-pressed={kinds.includes(k)}
							disabled={!counts[k] && !kinds.includes(k)}
							onclick={() => toggleKind(k)}
							class="inline-flex h-8 items-center gap-1.5 rounded-full border px-3 text-sm transition-colors disabled:opacity-40 {kinds.includes(k)
								? 'border-primary/50 bg-primary/10 text-primary'
								: 'hover:bg-accent/10'}"
						>
							{kindLabel(k)}
							<span class="text-xs tabular-nums text-muted-foreground">{counts[k]}</span>
						</button>
					{/each}
					<PlaylistSelect
						value={playlist || ALL}
						options={[
							{ value: ALL, label: t('alerts.all_playlists') },
							...alertPlaylistIds.map((id) => ({ value: id, label: nameOf(id) }))
						]}
						onpick={(v) => (playlist = v === ALL ? '' : v)}
						placeholder={t('alerts.all_playlists')}
						label={t('alerts.all_playlists')}
						class="w-56"
					/>
					<label class="ml-auto flex items-center gap-2 text-sm">
						<Switch bind:checked={unseenOnly} />
						{t('alerts.unseen_only')}
					</label>
				</div>
				{#if more}
					<p class="-mt-2 mb-4 text-xs text-muted-foreground">
						{t('alerts.more_to_load', { count: alerts.length })}
					</p>
				{/if}

				{#if !shown.length}
					<p class="text-sm text-muted-foreground">
						{t('alerts.empty_filtered')}
						{#if filtered}
							<Button variant="link" size="sm" onclick={clearFilters}>{t('alerts.clear_filters')}</Button>
						{/if}
					</p>
				{:else}
					{#each days as d (d.day)}
						<section class="mb-4">
							<h2 class="sticky top-0 z-10 mb-1 bg-background/90 py-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground backdrop-blur">
								{dayLabel(d.day)} · {d.items.length}
							</h2>
							<!-- Plain rows, no transition (docs/UI-PERFORMANCE.md). -->
							<ul>
								{#each d.items as a (a.id ?? `${a.playlist_id}:${a.video_id}:${a.kind}:${a.at}`)}
									<li
										class="flex items-center gap-3 rounded-lg px-2 py-1.5 [content-visibility:auto] [contain-intrinsic-size:auto_3.5rem] hover:bg-accent/10 {a.dismissed
											? 'opacity-60'
											: ''}"
									>
										<span class="w-2 shrink-0">
											{#if !a.seen}
												<span class="block h-2 w-2 rounded-full bg-primary"></span>
												<span class="sr-only">{t('alerts.unseen')}</span>
											{/if}
										</span>
										{@render songCell(a.song, a.video_id)}
										<span class="hidden shrink-0 items-center gap-1.5 md:flex">
											<Badge variant={kindVariant(a.kind)}>{kindLabel(a.kind)}</Badge>
											{#if a.kind === 'moved' && a.from !== undefined && a.to !== undefined}
												<span class="text-xs tabular-nums text-muted-foreground">
													{t('alerts.moved_to', { from: a.from + 1, to: a.to + 1 })}
												</span>
											{/if}
											{#if a.dismissed}
												<Badge variant="outline">{t('alerts.dismissed')}</Badge>
											{/if}
										</span>
										<a href={playlistHref(a.playlist_id)} class="hidden max-w-40 shrink-0 sm:block" title={t('alerts.go_to_playlist')}>
											<Badge variant="muted" class="max-w-40"><span class="truncate">{nameOf(a.playlist_id)}</span></Badge>
										</a>
										<span class="w-14 shrink-0 text-right text-xs tabular-nums text-muted-foreground">{timeOf(a.at)}</span>
										<span class="flex shrink-0 items-center">
											{@render songActions(a.song, a.kind, a.video_id)}
											<Button
												variant="ghost"
												size="icon-sm"
												onclick={() => goto(playlistHref(a.playlist_id))}
												title={t('alerts.go_to_playlist')}
												aria-label={t('alerts.go_to_playlist')}
											>
												<HugeiconsIcon icon={Playlist02Icon} class="h-4 w-4" />
											</Button>
											<Button
												variant="ghost"
												size="icon-sm"
												onclick={() => openTimeline(a.playlist_id)}
												title={t('alerts.view_timeline')}
												aria-label={t('alerts.view_timeline')}
											>
												<HugeiconsIcon icon={HistoryIcon} class="h-4 w-4" />
											</Button>
											<Button
												variant="ghost"
												size="icon-sm"
												class={a.seen || a.id === undefined ? 'invisible' : ''}
												disabled={a.seen || a.id === undefined}
												onclick={() => a.id !== undefined && markSeen([a.id])}
												title={t('alerts.mark_seen')}
												aria-label={t('alerts.mark_seen')}
											>
												<HugeiconsIcon icon={Tick02Icon} class="h-4 w-4" />
											</Button>
										</span>
									</li>
								{/each}
							</ul>
						</section>
					{/each}
				{/if}
				{#if more}
					<!-- The end of what is loaded: coming into view loads the next page. The button is
					     for when the observer can't tell (or the user is faster). -->
					<div class="flex justify-center py-4" {@attach endOfList}>
						<Button variant="outline" size="sm" disabled={loadingMore} onclick={loadMore}>
							{loadingMore ? t('common.loading') : t('alerts.load_more')}
						</Button>
					</div>
				{/if}
			{/if}
		</Tabs.Content>

		<Tabs.Content value="timeline">
			<div class="mb-4 flex flex-wrap items-center gap-2">
				<PlaylistSelect
					value={timelineFor}
					options={timelineOptions.map((id) => ({ value: id, label: nameOf(id) }))}
					onpick={(v) => (timelineFor = v)}
					placeholder={t('alerts.pick_playlist')}
					label={t('alerts.pick_playlist')}
					class="w-72"
				/>
				{#if timelineFor}
					<Button variant="ghost" size="sm" class="gap-1.5" href={playlistHref(timelineFor)}>
						<HugeiconsIcon icon={Playlist02Icon} class="h-4 w-4" />
						{t('alerts.go_to_playlist')}
					</Button>
				{/if}
			</div>

			{#if !timelineFor}
				<p class="text-sm text-muted-foreground">{t('alerts.timeline_pick')}</p>
			{:else if timelineLoading && !timeline.length}
				{#each Array(4) as _, i (i)}
					<div class="mb-2 h-14 animate-pulse rounded-lg bg-card/40"></div>
				{/each}
			{:else if !timeline.length}
				<p class="text-sm text-muted-foreground">{t('alerts.timeline_empty')}</p>
			{:else}
				<ol class="space-y-2">
					{#each timeline as e (e.snapshot_id)}
						{@const sums = timelineSums(e)}
						{@const expanded = open.has(e.snapshot_id)}
						<li class="overflow-hidden rounded-xl border">
							<button
								type="button"
								class="flex w-full items-center gap-3 px-3 py-2.5 text-left hover:bg-accent/10 disabled:cursor-default disabled:hover:bg-transparent"
								disabled={!e.changes.length}
								aria-expanded={e.changes.length ? expanded : undefined}
								onclick={() => toggleEntry(e.snapshot_id)}
							>
								<span class="min-w-0 flex-1">
									<span class="block text-xs text-muted-foreground">
										{whenOf(e.taken_at)} · {t('alerts.timeline_tracks', { count: e.item_count })}
									</span>
									<span class="mt-0.5 flex flex-wrap items-center gap-2 text-sm">
										{#if e.baseline}
											<span class="text-muted-foreground">{t('alerts.timeline_baseline')}</span>
										{:else if !sums.length}
											<span class="text-muted-foreground">{t('alerts.timeline_no_changes')}</span>
										{:else}
											{#each sums as s (s.kind)}
												<span class="tabular-nums" title={kindLabel(s.kind)}>
													<span class="font-semibold">{s.sign}{s.n}</span>
													<span class="text-muted-foreground">{kindLabel(s.kind)}</span>
												</span>
											{/each}
										{/if}
									</span>
								</span>
								{#if e.changes.length}
									<span class="sr-only">{expanded ? t('alerts.timeline_hide') : t('alerts.timeline_show')}</span>
									<!-- altIcon/showAlt, not a ternary: `icon` is read once at mount. -->
									<HugeiconsIcon icon={ArrowDown01Icon} altIcon={ArrowUp01Icon} showAlt={expanded} class="h-4 w-4 shrink-0 text-muted-foreground" />
								{/if}
							</button>
							{#if expanded}
								<ul class="border-t">
									{#each e.changes as c, i (i)}
										<li class="flex items-center gap-3 px-3 py-1.5 hover:bg-accent/10">
											<span class="w-24 shrink-0">
												<Badge variant={kindVariant(c.kind)}>{kindLabel(c.kind)}</Badge>
											</span>
											{@render songCell(c.song, c.video_id)}
											{#if c.kind === 'moved' && c.from !== undefined && c.to !== undefined}
												<span class="shrink-0 text-xs tabular-nums text-muted-foreground">
													{t('alerts.moved_to', { from: c.from + 1, to: c.to + 1 })}
												</span>
											{/if}
											<span class="flex shrink-0 items-center">
												{@render songActions(c.song, c.kind, c.video_id)}
											</span>
										</li>
									{/each}
								</ul>
							{/if}
						</li>
					{/each}
				</ol>
			{/if}
		</Tabs.Content>
	</Tabs.Root>
</div>
