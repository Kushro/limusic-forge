<script lang="ts">
	// The playlist page's facet filters (`facets.ts`), as a row of chips under the header: each one
	// says what it is set to, opens to change it, and clears with its ×. They narrow together with the
	// search box, which `.*` switches to a regular expression.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Cancel01Icon, ArrowDown01Icon } from '@hugeicons/core-free-icons';
	import type { SongItem } from '$lib/api';
	import { artistCounts, dayToSecs, facetsActive, NO_FACETS, secsToDay, type Facets } from '$lib/facets';
	import { t } from '$lib/i18n.svelte';
	import * as Popover from './ui/popover';
	import { Checkbox } from './ui/checkbox';
	import { Input } from './ui/input';

	// `global`: Library ▸ In your playlists, where each song is listed once with the playlists
	// holding it. Swaps "in here twice / also elsewhere" (one copy per song there) for which
	// playlists, availability, first seen, date added and spread; `playlists` lists the ones to
	// choose from.
	let {
		facets = $bindable(),
		regex = $bindable(false),
		regexError = false,
		items,
		global = false,
		playlists = [],
		downloads = false
	}: {
		facets: Facets;
		regex?: boolean;
		regexError?: boolean;
		items: SongItem[];
		global?: boolean;
		playlists?: { id: string; title: string; count: number }[];
		/** The host passes `FacetContext.downloaded`, so the Downloaded chip has something to go on. */
		downloads?: boolean;
	} = $props();

	let artistQuery = $state('');
	const counts = $derived(artistCounts(items));
	const shownArtists = $derived(
		counts.filter((a) => a.name.toLowerCase().includes(artistQuery.trim().toLowerCase())).slice(0, 200)
	);
	const artistLabel = $derived(
		facets.artists.length === 0
			? t('facets.artist')
			: facets.artists.length === 1
				? (counts.find((a) => a.key === facets.artists[0])?.name ?? t('facets.artist'))
				: t('facets.artists_n', { count: facets.artists.length })
	);
	const lengthLabel = $derived(
		facets.minMin === null && facets.maxMin === null
			? t('facets.length')
			: facets.maxMin === null
				? t('facets.length_min', { min: facets.minMin ?? 0 })
				: facets.minMin === null
					? t('facets.length_max', { max: facets.maxMin })
					: t('facets.length_range', { min: facets.minMin, max: facets.maxMin })
	);

	function toggleArtist(key: string, on: boolean) {
		facets = {
			...facets,
			artists: on ? [...facets.artists, key] : facets.artists.filter((k) => k !== key)
		};
	}
	const num = (v: string): number | null => (v.trim() === '' || isNaN(Number(v)) ? null : Math.max(0, Number(v)));

	const chip = (on: boolean) =>
		`inline-flex h-8 items-center gap-1.5 rounded-full border px-3 text-xs font-medium transition-colors ${
			on ? 'border-primary/40 bg-primary/15 text-primary' : 'text-muted-foreground hover:bg-accent/10 hover:text-foreground'
		}`;
	const DUPES: Facets['dupes'][] = ['all', 'repeated', 'elsewhere'];
	const KINDS: Facets['kind'][] = ['all', 'songs', 'videos'];
	const STATUSES: Facets['status'][] = ['all', 'available', 'unavailable'];
	const SPREADS: Facets['spread'][] = ['all', 'several', 'one'];
	const DOWNLOADED: Facets['downloaded'][] = ['all', 'yes', 'no'];
	function next<T>(all: T[], v: T): T {
		return all[(all.indexOf(v) + 1) % all.length];
	}

	let playlistQuery = $state('');
	const shownPlaylists = $derived(
		playlists.filter((p) => p.title.toLowerCase().includes(playlistQuery.trim().toLowerCase())).slice(0, 200)
	);
	const playlistLabel = $derived(
		facets.playlists.length === 0
			? t('facets.playlist')
			: facets.playlists.length === 1
				? (playlists.find((p) => p.id === facets.playlists[0])?.title ?? t('facets.playlist'))
				: t('facets.playlists_n', { count: facets.playlists.length })
	);
	function togglePlaylist(id: string, on: boolean) {
		facets = {
			...facets,
			playlists: on ? [...facets.playlists, id] : facets.playlists.filter((p) => p !== id)
		};
	}
	const day = (s: number) => new Date(s * 1000).toLocaleDateString();
	const seenLabel = $derived(
		facets.seenFrom === null && facets.seenTo === null
			? t('facets.seen')
			: facets.seenTo === null
				? t('facets.seen_from', { from: day(facets.seenFrom ?? 0) })
				: facets.seenFrom === null
					? t('facets.seen_to', { to: day(facets.seenTo) })
					: t('facets.seen_range', { from: day(facets.seenFrom), to: day(facets.seenTo) })
	);
	const addedLabel = $derived(
		facets.addedFrom === null && facets.addedTo === null
			? t('facets.added')
			: facets.addedTo === null
				? t('facets.added_from', { from: day(facets.addedFrom ?? 0) })
				: facets.addedFrom === null
					? t('facets.added_to', { to: day(facets.addedTo) })
					: t('facets.added_range', { from: day(facets.addedFrom), to: day(facets.addedTo) })
	);
</script>

<div class="flex flex-wrap items-center gap-2" role="toolbar" aria-label={t('facets.label')}>
	<Popover.Root>
		<Popover.Trigger class={chip(facets.artists.length > 0)}>
			{artistLabel}
			<HugeiconsIcon icon={ArrowDown01Icon} class="h-3 w-3" />
		</Popover.Trigger>
		<Popover.Content align="start" class="w-72 gap-2 p-2">
			<Input bind:value={artistQuery} placeholder={t('facets.find_artist')} class="h-8" />
			<div class="max-h-64 overflow-y-auto">
				{#each shownArtists as a (a.key)}
					<label class="flex cursor-pointer items-center gap-2 rounded-md px-1.5 py-1 text-sm hover:bg-accent/10">
						<Checkbox
							checked={facets.artists.includes(a.key)}
							onCheckedChange={(v) => toggleArtist(a.key, !!v)}
						/>
						<span class="min-w-0 flex-1 truncate">{a.name}</span>
						<span class="text-xs text-muted-foreground tabular-nums">{a.count}</span>
					</label>
				{/each}
			</div>
		</Popover.Content>
	</Popover.Root>

	<Popover.Root>
		<Popover.Trigger class={chip(facets.minMin !== null || facets.maxMin !== null)}>
			{lengthLabel}
			<HugeiconsIcon icon={ArrowDown01Icon} class="h-3 w-3" />
		</Popover.Trigger>
		<Popover.Content align="start" class="w-64 gap-2 p-3">
			<p class="text-xs text-muted-foreground">{t('facets.length_hint')}</p>
			<div class="flex items-center gap-2 text-sm">
				<Input
					type="number"
					min="0"
					class="h-8 w-20"
					value={facets.minMin ?? ''}
					oninput={(e) => (facets = { ...facets, minMin: num(e.currentTarget.value) })}
					aria-label={t('facets.from')}
				/>
				–
				<Input
					type="number"
					min="0"
					class="h-8 w-20"
					value={facets.maxMin ?? ''}
					oninput={(e) => (facets = { ...facets, maxMin: num(e.currentTarget.value) })}
					aria-label={t('facets.to')}
				/>
				{t('facets.minutes')}
			</div>
		</Popover.Content>
	</Popover.Root>

	{#if global}
		<Popover.Root>
			<Popover.Trigger class={chip(facets.playlists.length > 0)}>
				{playlistLabel}
				<HugeiconsIcon icon={ArrowDown01Icon} class="h-3 w-3" />
			</Popover.Trigger>
			<Popover.Content align="start" class="w-72 gap-2 p-2">
				<Input bind:value={playlistQuery} placeholder={t('facets.find_playlist')} class="h-8" />
				<div class="max-h-64 overflow-y-auto">
					{#each shownPlaylists as p (p.id)}
						<label class="flex cursor-pointer items-center gap-2 rounded-md px-1.5 py-1 text-sm hover:bg-accent/10">
							<Checkbox
								checked={facets.playlists.includes(p.id)}
								onCheckedChange={(v) => togglePlaylist(p.id, !!v)}
							/>
							<span class="min-w-0 flex-1 truncate">{p.title}</span>
							<span class="text-xs text-muted-foreground tabular-nums">{p.count}</span>
						</label>
					{/each}
				</div>
			</Popover.Content>
		</Popover.Root>

		<Popover.Root>
			<Popover.Trigger class={chip(facets.seenFrom !== null || facets.seenTo !== null)}>
				{seenLabel}
				<HugeiconsIcon icon={ArrowDown01Icon} class="h-3 w-3" />
			</Popover.Trigger>
			<Popover.Content align="start" class="w-80 gap-2 p-3">
				<p class="text-xs text-muted-foreground">{t('facets.seen_hint')}</p>
				<div class="flex items-center gap-2 text-sm">
					<Input
						type="date"
						class="h-8 flex-1"
						value={secsToDay(facets.seenFrom)}
						onchange={(e) => (facets = { ...facets, seenFrom: dayToSecs(e.currentTarget.value) })}
						aria-label={t('facets.from')}
					/>
					–
					<Input
						type="date"
						class="h-8 flex-1"
						value={secsToDay(facets.seenTo)}
						onchange={(e) => (facets = { ...facets, seenTo: dayToSecs(e.currentTarget.value, true) })}
						aria-label={t('facets.to')}
					/>
				</div>
			</Popover.Content>
		</Popover.Root>

		<!-- The real date added (Data API), falling back to first seen (`facets.addedDate`). -->
		<Popover.Root>
			<Popover.Trigger class={chip(facets.addedFrom !== null || facets.addedTo !== null)}>
				{addedLabel}
				<HugeiconsIcon icon={ArrowDown01Icon} class="h-3 w-3" />
			</Popover.Trigger>
			<Popover.Content align="start" class="w-80 gap-2 p-3">
				<p class="text-xs text-muted-foreground">{t('facets.added_hint')}</p>
				<div class="flex items-center gap-2 text-sm">
					<Input
						type="date"
						class="h-8 flex-1"
						value={secsToDay(facets.addedFrom)}
						onchange={(e) => (facets = { ...facets, addedFrom: dayToSecs(e.currentTarget.value) })}
						aria-label={t('facets.from')}
					/>
					–
					<Input
						type="date"
						class="h-8 flex-1"
						value={secsToDay(facets.addedTo)}
						onchange={(e) => (facets = { ...facets, addedTo: dayToSecs(e.currentTarget.value, true) })}
						aria-label={t('facets.to')}
					/>
				</div>
			</Popover.Content>
		</Popover.Root>
	{/if}

	<!-- Three-way facets cycle on click: few enough values that a menu would only slow them. -->
	{#if global}
		<button
			class={chip(facets.spread !== 'all')}
			onclick={() => (facets = { ...facets, spread: next(SPREADS, facets.spread) })}
			title={t('facets.spread_hint')}
		>
			{t(`facets.spread_${facets.spread}`)}
		</button>
		<button
			class={chip(facets.status !== 'all')}
			onclick={() => (facets = { ...facets, status: next(STATUSES, facets.status) })}
		>
			{t(`facets.status_${facets.status}`)}
		</button>
	{:else}
		<button
			class={chip(facets.dupes !== 'all')}
			onclick={() => (facets = { ...facets, dupes: next(DUPES, facets.dupes) })}
			title={t('facets.dupes_hint')}
		>
			{t(`facets.dupes_${facets.dupes}`)}
		</button>
	{/if}
	<button
		class={chip(facets.kind !== 'all')}
		onclick={() => (facets = { ...facets, kind: next(KINDS, facets.kind) })}
	>
		{t(`facets.kind_${facets.kind}`)}
	</button>
	{#if downloads}
		<button
			class={chip(facets.downloaded !== 'all')}
			onclick={() => (facets = { ...facets, downloaded: next(DOWNLOADED, facets.downloaded) })}
			title={t('facets.downloaded_hint')}
		>
			{t(`facets.downloaded_${facets.downloaded}`)}
		</button>
	{/if}
	<button
		class="{chip(regex)} font-mono {regexError ? 'border-destructive text-destructive' : ''}"
		onclick={() => (regex = !regex)}
		title={regexError ? t('facets.regex_error') : t('facets.regex_hint')}
		aria-pressed={regex}
	>
		.*
	</button>
	{#if facetsActive(facets) || regex}
		<button
			class="inline-flex h-8 items-center gap-1 rounded-full px-2 text-xs text-muted-foreground hover:text-foreground"
			onclick={() => {
				facets = { ...NO_FACETS };
				regex = false;
			}}
		>
			<HugeiconsIcon icon={Cancel01Icon} class="h-3.5 w-3.5" />
			{t('facets.clear')}
		</button>
	{/if}
</div>
