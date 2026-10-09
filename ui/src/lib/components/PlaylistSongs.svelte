<script lang="ts">
	// Library ▸ In your playlists: every song across your playlists once, with a chip per playlist
	// that holds it (PlaylistForge's global Videos screen). Tick songs to keep them in one playlist
	// only, or to take them out of all of them; both undo from the toast. Drag rows onto a sidebar
	// playlist to copy them there. The facet chips (`FilterChips` in its global mode) narrow by
	// playlist, availability, first seen, date added and spread, and the list sorts. Above the list, the
	// monitor's alerts: tracks that left a playlist or turned unavailable since the last sync.
	import { onMount, untrack } from 'svelte';
	import { goto } from '$app/navigation';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		Alert02Icon,
		Cancel01Icon,
		MusicNote01Icon,
		Search01Icon,
		SquareArrowRightDoubleIcon,
		Delete02Icon,
		Sorting01Icon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { BrowseItem, Everywhere, PlaylistAlert, SongItem } from '$lib/api';
	import { auth, library, personal, playback, toast } from '$lib/player.svelte';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { canDropOn } from '$lib/transfer.svelte';
	import { announceOp } from '$lib/playlistops.svelte';
	import {
		applyFacets,
		fold,
		NO_FACETS,
		playlistCounts,
		regexFilter,
		sortEverywhere,
		type EverywhereSort,
		type FacetContext,
		type Facets
	} from '$lib/facets';
	import { isDownloaded, track as trackDownloads } from '$lib/downloads.svelte';
	import { setDragRows } from '$lib/dnd';
	import { endRowDrag, startRowDrag } from '$lib/rowdrag.svelte';
	import { thumb } from '$lib/thumb';
	import { t } from '$lib/i18n.svelte';
	import { Badge } from './ui/badge';
	import { Button } from './ui/button';
	import { Checkbox } from './ui/checkbox';
	import * as Popover from './ui/popover';
	import { Input } from './ui/input';
	import * as Select from './ui/select';
	import TrackFilter from './TrackFilter.svelte';
	import FilterChips from './FilterChips.svelte';
	import YtDataWarning from './YtDataWarning.svelte';

	let { onalerts }: { onalerts?: (n: number) => void } = $props();

	let songs = $state.raw<Everywhere[]>([]);
	let alerts = $state.raw<PlaylistAlert[]>([]);
	// videoId → when it was added to a playlist, by the Data API (the date-added facet's real date).
	let addedDates = $state.raw<Record<string, number>>({});
	let loading = $state(true);
	let query = $state('');
	let facets = $state<Facets>({ ...NO_FACETS });
	let regex = $state(false);
	let sort = $state<EverywhereSort>('playlists');
	let limit = $state(200);
	let picked = $state<Set<string>>(new Set());
	let busy = $state(false);
	let confirmRemove = $state(false);
	let alertsOpen = $state(false);

	async function load() {
		try {
			[songs, alerts, addedDates] = await Promise.all([
				api.songsEverywhere(),
				api.playlistAlerts(),
				api.playlistAddedDates()
			]);
			onalerts?.(alerts.length);
		} catch (e) {
			toast.error(String(e));
		} finally {
			loading = false;
		}
	}
	onMount(() => {
		load();
		// A drop, a move or an undo anywhere changes what this shows.
		const off = api.onPlaylistsEdited(() => load());
		return () => void off.then((f) => f());
	});

	const playlists = $derived(orderLibrary(mergeSaved(personal, library.items, 'playlist'), personal));
	const names = $derived(new Map(playlists.map((p) => [p.id, p.title])));
	const nameOf = (id: string) => names.get(id) ?? t('common.playlist_singular');
	const targets = $derived(playlists.filter((p) => canDropOn(p, null)));
	let targetQuery = $state('');
	const shownTargets = $derived(
		targets.filter((p) => p.title.toLowerCase().includes(targetQuery.trim().toLowerCase()))
	);

	// Search box (plain, or a regex with `.*`), then the facet chips, then the chosen order.
	const byId = $derived(new Map(songs.map((e) => [e.song.video_id, e])));
	const allSongs = $derived(songs.map((e) => e.song));
	const ctx: FacetContext = {
		copies: new Map(),
		elsewhere: (v) => (byId.get(v)?.playlists.length ?? 0) > 1,
		playlistsOf: (v) => byId.get(v)?.playlists ?? [],
		firstSeen: (v) => byId.get(v)?.first_seen ?? null,
		addedAt: (v) => addedDates[v] ?? null,
		// Reactive through the store's map, so the list re-filters as the answers arrive.
		downloaded: isDownloaded
	};
	// The Downloaded facet needs every song's state, not just the rows scrolled into view.
	$effect(() => {
		const ids = songs.map((e) => e.song.video_id);
		untrack(() => trackDownloads(ids));
	});
	const searched = $derived.by((): { list: Everywhere[]; error: boolean } => {
		if (regex) {
			const r = regexFilter(allSongs, query);
			const keep = new Set(r.items);
			return { list: songs.filter((e) => keep.has(e.song)), error: r.error };
		}
		const q = fold(query.trim());
		if (!q) return { list: songs, error: false };
		return {
			list: songs.filter(
				(e) =>
					fold(e.song.title).includes(q) ||
					fold(e.song.artists ?? '').includes(q) ||
					e.playlists.some((p) => fold(nameOf(p)).includes(q))
			),
			error: false
		};
	});
	const shown = $derived.by(() => {
		const keep = new Set(applyFacets(searched.list.map((e) => e.song), facets, ctx));
		return sortEverywhere(searched.list.filter((e) => keep.has(e.song)), sort);
	});
	// The playlist facet's choices: the ones holding at least one song here, biggest first.
	const facetPlaylists = $derived(
		[...playlistCounts(songs)]
			.map(([id, count]) => ({ id, title: nameOf(id), count }))
			.sort((a, b) => b.count - a.count || a.title.localeCompare(b.title))
	);
	const SORTS = [
		{ value: 'playlists', label: 'everywhere.sort_playlists' },
		{ value: 'newest', label: 'sort.newest' },
		{ value: 'oldest', label: 'sort.oldest' },
		{ value: 'title', label: 'sort.title' },
		{ value: 'artist', label: 'sort.artist' }
	] as const satisfies readonly { value: EverywhereSort; label: string }[];
	const sortLabel = $derived(t((SORTS.find((o) => o.value === sort) ?? SORTS[0]).label));
	const pickedSongs = $derived(songs.filter((e) => picked.has(e.song.video_id)).map((e) => e.song));
	const allShownPicked = $derived(shown.every((e) => picked.has(e.song.video_id)));

	/** Tick every song the search and filters leave on screen (the ones not yet scrolled to too). */
	function selectFiltered() {
		picked = new Set(shown.map((e) => e.song.video_id));
		confirmRemove = false;
	}

	// A row dragged onto a sidebar playlist copies there (`from: null`: these come from many
	// playlists, so there is no one source to move them out of). Ticked rows go together, the ones
	// filtered out of sight included, when the dragged one is among them.
	function dragStart(e: DragEvent, song: SongItem) {
		if (!e.dataTransfer) return;
		const rows = picked.has(song.video_id) ? pickedSongs : [song];
		setDragRows(e, {
			from: null,
			fromTitle: t('everywhere.title'),
			// The index's row handle belongs to one playlist; a copy makes its own.
			rows: rows.map((s) => ({ song: { ...s, set_video_id: undefined }, before: null }))
		});
		e.dataTransfer.effectAllowed = 'copy';
		startRowDrag(rows.length, null);
	}

	function toggle(id: string, on: boolean) {
		const next = new Set(picked);
		if (on) next.add(id);
		else next.delete(id);
		picked = next;
		confirmRemove = false;
	}

	function play(i: number) {
		const list = shown.map((e) => e.song);
		api.playPlaylist(list, i, undefined, t('everywhere.title'), false).catch((e) => toast.error(String(e)));
	}

	async function keep(target: BrowseItem | null) {
		if (busy || !pickedSongs.length) return;
		busy = true;
		const titles = Object.fromEntries(names);
		try {
			const r = await api.keepOnlyIn(pickedSongs, target && { id: target.id, title: target.title }, titles);
			const n = pickedSongs.length;
			const msg = target
				? n === 1
					? t('everywhere.kept_one', { playlist: target.title })
					: t('everywhere.kept', { count: n, playlist: target.title })
				: r.removed === 1
					? t('everywhere.removed_one')
					: t('everywhere.removed', { count: r.removed });
			announceOp(r.op, r.failed.length ? `${msg} · ${t('everywhere.failed', { count: r.failed.length })}` : msg);
			picked = new Set();
			confirmRemove = false;
		} catch (e) {
			toast.error(String(e));
		} finally {
			busy = false;
		}
	}

	async function dismiss(a: PlaylistAlert) {
		await api.dismissPlaylistAlert(a).catch(() => {});
		alerts = alerts.filter((x) => x !== a);
		onalerts?.(alerts.length);
	}

	async function removeFromItsPlaylist(a: PlaylistAlert) {
		if (!a.song?.set_video_id) return;
		try {
			const op = await api.removeTracks(a.playlist_id, nameOf(a.playlist_id), [{ song: a.song, before: null }]);
			announceOp(op, t('toasts.removed_from_playlist'));
			await dismiss(a);
		} catch (e) {
			toast.error(String(e));
		}
	}

	// The alerts page, filtered to the one playlist these are about when they are all about one.
	const alertsHref = $derived.by(() => {
		const ids = new Set(alerts.map((a) => a.playlist_id));
		const [only] = ids;
		return ids.size === 1 ? `/alerts?playlist=${encodeURIComponent(only)}` : '/alerts';
	});

	const findIt = (s: SongItem | null) =>
		s && goto(`/search?q=${encodeURIComponent(`${s.title} ${s.artists ?? ''}`.trim())}`);

	// One step at a time down the list, like Library ▸ Songs: a few thousand rows at once is a
	// stall on open, and nobody reads past the first screen without scrolling there.
	function more(node: HTMLElement) {
		const io = new IntersectionObserver(([e]) => e.isIntersecting && (limit += 200), {
			rootMargin: '600px 0px'
		});
		io.observe(node);
		return () => io.disconnect();
	}
</script>

{#if loading}
	<div class="mb-4 h-24 animate-pulse rounded-2xl border bg-card/40"></div>
{:else}
	<!-- The dates added and unavailability reasons here come from syncs through the Data API. -->
	<YtDataWarning />
	{#if alerts.length}
		<section class="mb-4 rounded-xl border border-destructive/30 bg-destructive/5 p-3">
			<div class="flex items-center gap-3">
				<button class="flex min-w-0 flex-1 items-center gap-2 text-left text-sm font-medium" onclick={() => (alertsOpen = !alertsOpen)}>
					<HugeiconsIcon icon={Alert02Icon} class="h-4 w-4 text-destructive" />
					<span class="flex-1">
						{alerts.length === 1 ? t('everywhere.alerts_one') : t('everywhere.alerts', { count: alerts.length })}
					</span>
					<span class="text-xs text-muted-foreground">{alertsOpen ? t('common.less') : t('common.more')}</span>
				</button>
				<!-- Every alert ever filed, repeats and dismissed ones included, with each playlist's history. -->
				<a href={alertsHref} class="shrink-0 text-xs font-medium text-primary hover:underline">{t('everywhere.view_all')}</a>
			</div>
			{#if alertsOpen}
				<ul class="mt-2 space-y-1">
					{#each alerts as a (a.playlist_id + a.video_id + a.kind)}
						<li class="flex items-center gap-3 rounded-md px-1 py-1 text-sm">
							<Badge variant={a.kind === 'removed' ? 'muted' : 'label'}>{t(`everywhere.kind_${a.kind}`)}</Badge>
							<span class="min-w-0 flex-1 truncate">
								{a.song?.title ?? a.video_id}
								<span class="text-muted-foreground"> · {a.song?.artists ?? ''} · {nameOf(a.playlist_id)}</span>
							</span>
							{#if a.song}
								<Button variant="ghost" size="icon-sm" onclick={() => findIt(a.song)} title={t('everywhere.find')} aria-label={t('everywhere.find')}>
									<HugeiconsIcon icon={Search01Icon} class="h-4 w-4" />
								</Button>
							{/if}
							{#if a.kind === 'unavailable' && a.song?.set_video_id}
								<Button variant="ghost" size="icon-sm" onclick={() => removeFromItsPlaylist(a)} title={t('selection.remove')} aria-label={t('selection.remove')}>
									<HugeiconsIcon icon={Delete02Icon} class="h-4 w-4" />
								</Button>
							{/if}
							<Button variant="ghost" size="icon-sm" onclick={() => dismiss(a)} title={t('everywhere.dismiss')} aria-label={t('everywhere.dismiss')}>
								<HugeiconsIcon icon={Cancel01Icon} class="h-4 w-4" />
							</Button>
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	{/if}

	<div class="mb-3 flex flex-wrap items-center gap-3">
		<p class="text-sm text-muted-foreground">{t('everywhere.intro', { count: songs.length })}</p>
		<Select.Root type="single" value={sort} onValueChange={(v) => v && (sort = v as EverywhereSort)}>
			<Select.Trigger size="sm" class="ml-auto w-48" aria-label={t('sort.label')}>
				<HugeiconsIcon icon={Sorting01Icon} class="h-4 w-4 shrink-0" />
				<span class="flex-1 truncate text-left">{sortLabel}</span>
			</Select.Trigger>
			<Select.Content>
				{#each SORTS as o (o.value)}
					<Select.Item value={o.value} label={t(o.label)}>{t(o.label)}</Select.Item>
				{/each}
			</Select.Content>
		</Select.Root>
		<TrackFilter bind:value={query} placeholder={t('everywhere.search')} />
	</div>
	<div class="mb-3 flex flex-wrap items-center gap-2">
		<FilterChips bind:facets bind:regex regexError={searched.error} items={allSongs} global playlists={facetPlaylists} downloads />
		{#if shown.length && !allShownPicked}
			<Button variant="ghost" size="sm" class="ml-auto" onclick={selectFiltered}>
				{t('everywhere.select_filtered', { count: shown.length })}
			</Button>
		{/if}
	</div>

	{#if pickedSongs.length}
		<!-- Sticky inside the page's scroll, so the actions stay in reach while ticking down a list. -->
		<div class="sticky top-0 z-20 mb-2 flex flex-wrap items-center gap-2 rounded-xl border bg-card px-3 py-2 text-sm shadow-sm" role="status">
			<span class="font-medium">{t('selection.count', { count: pickedSongs.length })}</span>
			<Popover.Root>
				<Popover.Trigger disabled={busy} class="inline-flex h-8 items-center gap-1.5 rounded-full border px-3 text-sm hover:bg-accent/10">
					<HugeiconsIcon icon={SquareArrowRightDoubleIcon} class="h-4 w-4" />
					{t('everywhere.keep_only')}
				</Popover.Trigger>
				<Popover.Content align="start" class="w-64 gap-2 p-2">
					<Input bind:value={targetQuery} placeholder={t('drop.search')} aria-label={t('drop.search')} class="h-8" />
					<div class="max-h-64 overflow-y-auto">
						{#each shownTargets as p (p.id)}
							<button class="w-full truncate rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent/10" onclick={() => keep(p)}>
								{p.title}
							</button>
						{:else}
							<p class="px-2 py-3 text-sm text-muted-foreground">
								{targets.length ? t('common.no_matches') : t('drop.no_targets')}
							</p>
						{/each}
					</div>
				</Popover.Content>
			</Popover.Root>
			{#if confirmRemove}
				<Button variant="destructive" size="sm" disabled={busy} onclick={() => keep(null)}>
					{t('everywhere.remove_confirm', { count: pickedSongs.length })}
				</Button>
			{:else}
				<Button variant="ghost" size="sm" class="gap-1.5" disabled={busy} onclick={() => (confirmRemove = true)}>
					<HugeiconsIcon icon={Delete02Icon} class="h-4 w-4" />
					{t('everywhere.remove_all')}
				</Button>
			{/if}
			<span class="hidden text-xs text-muted-foreground lg:inline">{t('everywhere.drag_hint')}</span>
			<Button variant="ghost" size="sm" class="ml-auto" onclick={() => ((picked = new Set()), (confirmRemove = false))}>
				{t('selection.clear')}
			</Button>
		</div>
	{/if}

	{#if !songs.length}
		<p class="text-sm text-muted-foreground">
			{auth.account?.signedIn ? t('everywhere.empty') : t('everywhere.empty_signed_out')}
		</p>
	{:else if !shown.length}
		<p class="text-sm text-muted-foreground">{t('common.no_matches')}</p>
	{:else}
		<!-- Plain rows, no transition (docs/UI-PERFORMANCE.md), and the browser skips the ones off
		     screen (`content-visibility`). -->
		<div role="list">
			{#each shown.slice(0, limit) as e, i (e.song.video_id)}
				{@const s = e.song}
				<div
					role="listitem"
					draggable="true"
					ondragstart={(ev) => dragStart(ev, s)}
					ondragend={endRowDrag}
					class="flex items-center gap-3 rounded-lg px-2 py-1.5 [content-visibility:auto] [contain-intrinsic-size:auto_3.5rem] hover:bg-accent/10 {s.video_id === playback.now?.videoId ? 'text-primary' : ''}"
				>
					<Checkbox
						checked={picked.has(s.video_id)}
						onCheckedChange={(v) => toggle(s.video_id, !!v)}
						aria-label={t('selection.select_track', { title: s.title })}
					/>
					<button class="flex min-w-0 flex-1 items-center gap-3 text-left" onclick={() => play(i)}>
						{#if s.thumbnail}
							<img src={thumb(s.thumbnail, 96)} alt="" loading="lazy" class="h-10 w-10 shrink-0 rounded-md object-cover" />
						{:else}
							<span class="flex h-10 w-10 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground/50">
								<HugeiconsIcon icon={MusicNote01Icon} class="h-4 w-4" />
							</span>
						{/if}
						<span class="min-w-0 flex-1">
							<span class="block truncate text-sm font-medium">{s.title}</span>
							<span class="block truncate text-xs text-muted-foreground">{s.artists}</span>
						</span>
					</button>
					<!-- Two chips and a count, PlaylistForge's rule: enough to recognise, never a wall. -->
					<span class="hidden shrink-0 items-center gap-1 sm:flex">
						{#each e.playlists.slice(0, 2) as p (p)}
							<a href="/playlist/{encodeURIComponent(p)}" class="max-w-36">
								<Badge variant="muted" class="max-w-36"><span class="truncate">{nameOf(p)}</span></Badge>
							</a>
						{/each}
						{#if e.playlists.length > 2}
							<Badge variant="outline" title={e.playlists.slice(2).map(nameOf).join(', ')}>+{e.playlists.length - 2}</Badge>
						{/if}
					</span>
					<span class="w-12 shrink-0 text-right text-xs text-muted-foreground tabular-nums">{s.duration ?? ''}</span>
				</div>
			{/each}
		</div>
		{#if shown.length > limit}
			<div {@attach more} class="h-8"></div>
		{/if}
	{/if}
{/if}
