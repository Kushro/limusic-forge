<script lang="ts">
	// The playlist page's facet filters (`facets.ts`), as a row of chips under the header: each one
	// says what it is set to, opens to change it, and clears with its ×. They narrow together with the
	// search box, which `.*` switches to a regular expression.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Cancel01Icon, ArrowDown01Icon } from '@hugeicons/core-free-icons';
	import type { SongItem } from '$lib/api';
	import { artistCounts, facetsActive, NO_FACETS, type Facets } from '$lib/facets';
	import { t } from '$lib/i18n.svelte';
	import * as Popover from './ui/popover';
	import { Checkbox } from './ui/checkbox';
	import { Input } from './ui/input';

	let {
		facets = $bindable(),
		regex = $bindable(false),
		regexError = false,
		items
	}: { facets: Facets; regex?: boolean; regexError?: boolean; items: SongItem[] } = $props();

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

	<!-- Three-way facets cycle on click: few enough values that a menu would only slow them. -->
	<button
		class={chip(facets.dupes !== 'all')}
		onclick={() => (facets = { ...facets, dupes: DUPES[(DUPES.indexOf(facets.dupes) + 1) % DUPES.length] })}
		title={t('facets.dupes_hint')}
	>
		{t(`facets.dupes_${facets.dupes}`)}
	</button>
	<button
		class={chip(facets.kind !== 'all')}
		onclick={() => (facets = { ...facets, kind: KINDS[(KINDS.indexOf(facets.kind) + 1) % KINDS.length] })}
	>
		{t(`facets.kind_${facets.kind}`)}
	</button>
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
