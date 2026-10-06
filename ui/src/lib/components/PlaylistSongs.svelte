<script lang="ts">
	// Library ▸ In your playlists: every song across your playlists once, with a chip per playlist
	// that holds it (PlaylistForge's global Videos screen). Tick songs to keep them in one playlist
	// only, or to take them out of all of them; both undo from the toast. Above the list, the
	// monitor's alerts: tracks that left a playlist or turned unavailable since the last sync.
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		Alert02Icon,
		Cancel01Icon,
		MusicNote01Icon,
		Search01Icon,
		SquareArrowRightDoubleIcon,
		Delete02Icon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { BrowseItem, Everywhere, PlaylistAlert, SongItem } from '$lib/api';
	import { auth, library, personal, playback, toast } from '$lib/player.svelte';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { canDropOn } from '$lib/transfer.svelte';
	import { announceOp } from '$lib/playlistops.svelte';
	import { fold } from '$lib/facets';
	import { thumb } from '$lib/thumb';
	import { t } from '$lib/i18n.svelte';
	import { Badge } from './ui/badge';
	import { Button } from './ui/button';
	import { Checkbox } from './ui/checkbox';
	import { Switch } from './ui/switch';
	import * as Popover from './ui/popover';
	import TrackFilter from './TrackFilter.svelte';

	let { onalerts }: { onalerts?: (n: number) => void } = $props();

	let songs = $state.raw<Everywhere[]>([]);
	let alerts = $state.raw<PlaylistAlert[]>([]);
	let loading = $state(true);
	let query = $state('');
	let shared = $state(false);
	let limit = $state(200);
	let picked = $state<Set<string>>(new Set());
	let busy = $state(false);
	let confirmRemove = $state(false);
	let alertsOpen = $state(false);

	async function load() {
		try {
			[songs, alerts] = await Promise.all([api.songsEverywhere(), api.playlistAlerts()]);
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

	const shown = $derived.by(() => {
		const q = fold(query.trim());
		return songs.filter(
			(e) =>
				(!shared || e.playlists.length > 1) &&
				(!q ||
					fold(e.song.title).includes(q) ||
					fold(e.song.artists ?? '').includes(q) ||
					e.playlists.some((p) => fold(nameOf(p)).includes(q)))
		);
	});
	const pickedSongs = $derived(songs.filter((e) => picked.has(e.song.video_id)).map((e) => e.song));

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
	{#if alerts.length}
		<section class="mb-4 rounded-xl border border-destructive/30 bg-destructive/5 p-3">
			<button class="flex w-full items-center gap-2 text-left text-sm font-medium" onclick={() => (alertsOpen = !alertsOpen)}>
				<HugeiconsIcon icon={Alert02Icon} class="h-4 w-4 text-destructive" />
				<span class="flex-1">
					{alerts.length === 1 ? t('everywhere.alerts_one') : t('everywhere.alerts', { count: alerts.length })}
				</span>
				<span class="text-xs text-muted-foreground">{alertsOpen ? t('common.less') : t('common.more')}</span>
			</button>
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
		<label class="ml-auto flex items-center gap-2 text-sm">
			<Switch bind:checked={shared} />
			{t('everywhere.shared')}
		</label>
		<TrackFilter bind:value={query} placeholder={t('everywhere.search')} />
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
				<Popover.Content align="start" class="max-h-72 w-64 overflow-y-auto p-1">
					{#each targets as p (p.id)}
						<button class="w-full truncate rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent/10" onclick={() => keep(p)}>
							{p.title}
						</button>
					{:else}
						<p class="px-2 py-3 text-sm text-muted-foreground">{t('drop.no_targets')}</p>
					{/each}
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
