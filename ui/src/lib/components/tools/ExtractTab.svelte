<script lang="ts">
	// Extract by filter (PlaylistForge's "extraer por filtro"): the tracks of a playlist that match a
	// search, a regular expression or the facet chips, copied or moved into a playlist you have or a
	// new one. Into one you have it is the same transfer a drop on a sidebar playlist makes
	// (`transfer.svelte.ts`). Into a new one, `tools.extract_new_mode` decides: `build` (default)
	// makes the playlist and fills it as one undoable change (`playlist_tools/build.rs`, kind
	// `extract`), `create_transfer` makes the playlist and then transfers into it.
	import { onMount } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { FilterIcon } from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { SongItem } from '$lib/api';
	import {
		auth,
		bumpLibraryTrackCount,
		createLibraryPlaylist,
		library,
		personal,
		savedIn,
		toast
	} from '$lib/player.svelte';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { building, runBuild, stopBuild } from '$lib/build.svelte';
	import { canDropOn, transfer } from '$lib/transfer.svelte';
	import { applyFacets, facetsActive, NO_FACETS, regexFilter, type Facets } from '$lib/facets';
	import { anchorsFor } from '$lib/reorder';
	import { t } from '$lib/i18n.svelte';
	import { Button } from '../ui/button';
	import { Input } from '../ui/input';
	import { Switch } from '../ui/switch';
	import * as RadioGroup from '../ui/radio-group';
	import FilterChips from '../FilterChips.svelte';
	import { filterTracks } from '../TrackFilter.svelte';
	import BuildProgress from './BuildProgress.svelte';

	let {
		playlistId,
		title,
		editable,
		ondone
	}: { playlistId: string; title: string; editable: boolean; ondone: () => void } = $props();

	type NewMode = 'build' | 'create_transfer';
	const NEW_MODES: NewMode[] = ['build', 'create_transfer'];
	const SHOWN = 200;

	let rows = $state.raw<SongItem[] | null>(null);
	let loadError = $state<string | null>(null);
	let query = $state('');
	let regex = $state(false);
	let facets = $state<Facets>({ ...NO_FACETS });
	let mode = $state<'copy' | 'move'>('copy');
	let into = $state<'new' | 'existing'>('new');
	let name = $state('');
	let targetFilter = $state('');
	let targetId = $state<string | null>(null);
	let skipExisting = $state(true);
	let newMode = $state<NewMode>('build');
	let working = $state(false);
	const signedIn = $derived(!!auth.account?.signedIn);
	let local = $state(false);
	$effect(() => {
		local = api.isLocalPlaylist(playlistId) || !signedIn;
	});
	$effect(() => {
		if (!editable) mode = 'copy';
	});

	onMount(() => {
		name = t('extract.default_name', { playlist: title });
		api
			.getSettings()
			.then((s) => {
				newMode = s['tools.extract_new_mode'] === 'create_transfer' ? 'create_transfer' : 'build';
			})
			.catch(() => {});
	});

	function chooseNewMode(v: NewMode) {
		newMode = v;
		api.setSetting('tools.extract_new_mode', v).catch(() => {});
	}

	// The whole playlist, every page, with each row's handle: a move takes those exact rows out.
	$effect(() => {
		const pid = playlistId;
		rows = null;
		loadError = null;
		api
			.playlistRows(pid)
			.then((r) => {
				if (pid === playlistId) rows = r;
			})
			.catch((e) => {
				if (pid === playlistId) loadError = String(e);
			});
	});

	const copies = $derived.by(() => {
		const n = new Map<string, number>();
		for (const s of rows ?? []) n.set(s.video_id, (n.get(s.video_id) ?? 0) + 1);
		return n;
	});
	const searched = $derived(
		regex ? regexFilter(rows ?? [], query) : { items: filterTracks(rows ?? [], query), error: false }
	);
	const matched = $derived(
		applyFacets(searched.items, facets, {
			copies,
			elsewhere: (v) =>
				(savedIn.map[v] ?? []).some((p) => p !== playlistId && p !== api.LIKED_MUSIC_ID)
		})
	);
	// Nothing narrowed: the whole playlist would go, which is a copy of it, not an extract.
	const narrowed = $derived(!!query.trim() || facetsActive(facets));

	// Same list and order as the sidebar; only playlists a drop could go into.
	const targets = $derived(
		orderLibrary(mergeSaved(personal, library.items, 'playlist'), personal).filter((p) =>
			canDropOn(p, playlistId)
		)
	);
	const targetMatches = $derived(
		targets.filter((p) => p.title.toLowerCase().includes(targetFilter.trim().toLowerCase()))
	);
	const target = $derived(targets.find((p) => p.id === targetId) ?? null);

	const ready = $derived(
		!!rows &&
			matched.length > 0 &&
			!searched.error &&
			(into === 'new' ? !!name.trim() : !!target) &&
			!working
	);

	/** The matched rows as a drag from this playlist: each with the row after it that stays. */
	function asDrag(songs: SongItem[]) {
		const handleOf = (s: SongItem) => s.set_video_id ?? '';
		const anchors = anchorsFor((rows ?? []).map(handleOf), new Set(songs.map(handleOf)));
		return {
			from: editable ? playlistId : null,
			fromTitle: title,
			rows: songs.map((song) => ({ song, before: anchors.get(handleOf(song)) ?? null }))
		};
	}

	async function run() {
		if (!ready || !rows) return;
		const songs = matched;
		const how = editable ? mode : 'copy';
		working = true;
		try {
			if (into === 'existing' && target) {
				if (await transfer(asDrag(songs), target, how, skipExisting ? 'skip' : 'allow')) ondone();
				return;
			}
			const playlist = name.trim();
			if (newMode === 'create_transfer') {
				let made: api.BrowseItem;
				try {
					made = await createLibraryPlaylist(playlist, local);
				} catch (e) {
					toast.error(String(e));
					return;
				}
				// A brand-new playlist holds nothing, so there are no duplicates to weigh.
				if (await transfer(asDrag(songs), made, how, 'allow')) ondone();
				return;
			}
			const b = await runBuild(
				{
					kind: 'extract',
					sources: [{ id: playlistId, title }],
					lists: [{ name: playlist, songs }],
					dest: { to: 'new', local },
					mode: how
				},
				(b) =>
					b.removed
						? t('extract.done_moved', { count: b.removed, playlist })
						: t('extract.done_copied', { count: b.added, playlist })
			);
			if (b?.removed) bumpLibraryTrackCount(playlistId, -b.removed);
			if (b && !b.error) ondone();
		} finally {
			working = false;
		}
	}
</script>

{#if building.running}
	<BuildProgress onstop={stopBuild} />
{:else}
	<div class="flex min-h-0 flex-col gap-3">
		<p class="text-sm text-muted-foreground">{t('extract.intro', { playlist: title })}</p>
		{#if loadError}
			<p class="text-sm text-destructive" role="alert">{loadError}</p>
		{:else if !rows}
			<p class="text-sm text-muted-foreground" role="status">{t('extract.loading')}</p>
		{:else}
			<Input
				bind:value={query}
				placeholder={t('extract.search')}
				aria-label={t('extract.search')}
				aria-invalid={searched.error || undefined}
			/>
			<FilterChips bind:facets bind:regex regexError={searched.error} items={rows} />
			{#if searched.error}
				<p class="text-xs text-destructive" role="alert">{t('extract.regex_error')}</p>
			{/if}
			<p class="flex items-center gap-2 text-sm" role="status">
				<HugeiconsIcon icon={FilterIcon} class="h-4 w-4 text-muted-foreground" />
				{matched.length === 1
					? t('extract.matches_one', { total: rows.length })
					: t('extract.matches', { count: matched.length, total: rows.length })}
			</p>
			{#if narrowed}
				<ol class="max-h-[22vh] list-decimal space-y-0.5 overflow-y-auto rounded-lg border py-1 pl-8 pr-1 text-sm">
					{#each matched.slice(0, SHOWN) as s, i (i)}
						<li class="truncate"><span>{s.title}</span> <span class="text-muted-foreground">· {s.artists}</span></li>
					{:else}
						<li class="list-none text-muted-foreground">{t('extract.none')}</li>
					{/each}
				</ol>
				{#if matched.length > SHOWN}
					<p class="text-xs text-muted-foreground">{t('extract.more', { count: matched.length - SHOWN })}</p>
				{/if}
			{/if}

			<div class="grid gap-3 sm:grid-cols-2">
				<div>
					<p class="mb-1 text-xs font-medium text-muted-foreground">{t('extract.mode')}</p>
					<RadioGroup.Root value={mode} onValueChange={(v) => (mode = v as typeof mode)} class="gap-1">
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<RadioGroup.Item value="copy" />{t('extract.copy')}
						</label>
						<label class="flex cursor-pointer items-center gap-2 text-sm {editable ? '' : 'opacity-50'}">
							<RadioGroup.Item value="move" disabled={!editable} />{t('extract.move')}
						</label>
					</RadioGroup.Root>
					<p class="mt-1 text-xs text-muted-foreground">
						{editable ? t('extract.move_hint', { playlist: title }) : t('extract.not_editable')}
					</p>
				</div>
				<div>
					<p class="mb-1 text-xs font-medium text-muted-foreground">{t('extract.into')}</p>
					<RadioGroup.Root value={into} onValueChange={(v) => (into = v as typeof into)} class="gap-1">
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<RadioGroup.Item value="new" />{t('extract.into_new')}
						</label>
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<RadioGroup.Item value="existing" />{t('extract.into_existing')}
						</label>
					</RadioGroup.Root>
				</div>
			</div>

			{#if into === 'new'}
				<Input bind:value={name} aria-label={t('extract.name')} placeholder={t('extract.name')} />
				{#if signedIn && !api.isLocalPlaylist(playlistId)}
					<label class="flex items-center gap-2 text-sm">
						<Switch bind:checked={local} />
						{t('build.keep_local')}
					</label>
				{/if}
				<div>
					<p class="mb-1 text-xs font-medium text-muted-foreground">{t('tools.extract_new_mode')}</p>
					<RadioGroup.Root value={newMode} onValueChange={(v) => chooseNewMode(v as NewMode)} class="gap-1">
						{#each NEW_MODES as m (m)}
							<label class="flex cursor-pointer items-start gap-2 text-sm">
								<RadioGroup.Item value={m} class="mt-0.5" />
								<span>
									{t(`tools.mode_${m}`)}
									<span class="block text-xs text-muted-foreground">{t(`tools.mode_${m}_hint`)}</span>
								</span>
							</label>
						{/each}
					</RadioGroup.Root>
				</div>
			{:else}
				<Input bind:value={targetFilter} placeholder={t('extract.target')} aria-label={t('extract.target')} />
				<div class="max-h-36 overflow-y-auto rounded-lg border p-1">
					<RadioGroup.Root value={targetId ?? ''} onValueChange={(v) => (targetId = v || null)} class="gap-0">
						{#each targetMatches as p (p.id)}
							<label class="flex cursor-pointer items-center gap-3 rounded-md px-2 py-1.5 text-sm hover:bg-accent/10">
								<RadioGroup.Item value={p.id} />
								<span class="min-w-0 flex-1 truncate">{p.title}</span>
							</label>
						{:else}
							<p class="px-2 py-3 text-sm text-muted-foreground">{t('extract.no_targets')}</p>
						{/each}
					</RadioGroup.Root>
				</div>
				<label class="flex items-center gap-2 text-sm">
					<Switch bind:checked={skipExisting} />
					{t('extract.skip_existing')}
				</label>
			{/if}

			<div class="flex justify-end">
				<Button onclick={run} disabled={!ready}>
					{mode === 'move' && editable
						? matched.length === 1
							? t('extract.move_one')
							: t('extract.move_n', { count: matched.length })
						: matched.length === 1
							? t('extract.copy_one')
							: t('extract.copy_n', { count: matched.length })}
				</Button>
			</div>
		{/if}
	</div>
{/if}
