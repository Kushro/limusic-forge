<script lang="ts">
	// Split a playlist into several (`playlist_tools/split.rs`): one per artist, or into parts by
	// count or size. The preview names every part and lets you rename or drop one before anything
	// is made; the source playlist is never changed.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { ArrowLeft01Icon, GitForkIcon } from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { SplitBy, SplitOrder, SplitPlan } from '$lib/api';
	import { auth, toast } from '$lib/player.svelte';
	import { building, runBuild, stopBuild } from '$lib/build.svelte';
	import { t } from '$lib/i18n.svelte';
	import { Button } from '../ui/button';
	import { Checkbox } from '../ui/checkbox';
	import { Input } from '../ui/input';
	import { Switch } from '../ui/switch';
	import * as RadioGroup from '../ui/radio-group';
	import BuildProgress from './BuildProgress.svelte';

	let {
		playlistId,
		title,
		ondone
	}: { playlistId: string; title: string; ondone: () => void } = $props();

	type Mode = 'artist' | 'count' | 'size';
	type OrderKey = 'playlist' | 'title' | 'artist' | 'duration' | 'shuffle';
	const MODES: Mode[] = ['artist', 'count', 'size'];
	const ORDERS: OrderKey[] = ['playlist', 'title', 'artist', 'duration', 'shuffle'];

	let mode = $state<Mode>('artist');
	let min = $state(3);
	let parts = $state(2);
	let max = $state(100);
	let order = $state<OrderKey>('playlist');
	let seed = $state(Math.floor(Math.random() * 1_000_000));
	let planning = $state(false);
	let plan = $state.raw<SplitPlan | null>(null);
	let names = $state<string[]>([]);
	let include = $state<boolean[]>([]);
	// Where the new playlists live: the account when signed in, unless the source is on this
	// machine already, which keeps its parts there too.
	const signedIn = $derived(!!auth.account?.signedIn);
	let local = $state(false);
	$effect(() => {
		local = api.isLocalPlaylist(playlistId) || !signedIn;
	});

	async function preview() {
		if (planning) return;
		planning = true;
		const by: SplitBy =
			mode === 'artist'
				? { by: 'artist', min: Math.max(1, min) }
				: mode === 'count'
					? { by: 'count', parts: Math.max(2, parts) }
					: { by: 'size', max: Math.max(1, max) };
		const ord: SplitOrder = order === 'shuffle' ? { order, seed } : { order };
		try {
			const p = await api.planSplit(playlistId, by, ord);
			plan = p;
			names = p.parts.map((part, i) =>
				part.artist
					? part.artist
					: mode === 'artist'
						? t('split.various', { playlist: title })
						: t('split.numbered', { playlist: title, n: i + 1, total: p.parts.length })
			);
			include = p.parts.map(() => true);
		} catch (e) {
			toast.error(String(e));
		} finally {
			planning = false;
		}
	}

	const chosen = $derived(plan ? plan.parts.filter((_, i) => include[i]) : []);

	async function create() {
		if (!plan || !chosen.length) return;
		const p = plan;
		const lists = p.parts
			.map((part, i) => ({ part, i }))
			.filter(({ i }) => include[i])
			.map(({ part, i }) => ({
				name: names[i].trim() || t('split.numbered', { playlist: title, n: i + 1, total: p.parts.length }),
				songs: part.rows.map((r) => p.rows[r])
			}));
		const b = await runBuild(
			{ kind: 'split', sources: [{ id: playlistId, title }], lists, dest: { to: 'new', local } },
			(b) => t('split.done', { count: b.created.length })
		);
		if (b && !b.error) ondone();
	}
</script>

{#if building.running}
	<BuildProgress onstop={stopBuild} />
{:else if !plan}
	<div class="space-y-4">
		<p class="text-sm text-muted-foreground">{t('split.intro')}</p>
		<RadioGroup.Root value={mode} onValueChange={(v) => (mode = v as Mode)} class="gap-2">
			{#each MODES as m (m)}
				<label class="flex cursor-pointer items-center gap-3 rounded-lg border p-3 hover:bg-accent/5">
					<RadioGroup.Item value={m} />
					<span class="flex-1 text-sm">{t(`split.by_${m}`)}</span>
					{#if m === 'artist' && mode === 'artist'}
						<span class="flex items-center gap-2 text-xs text-muted-foreground">
							{t('split.min')}
							<Input type="number" min="1" class="h-8 w-16" bind:value={min} />
						</span>
					{:else if m === 'count' && mode === 'count'}
						<Input type="number" min="2" class="h-8 w-16" bind:value={parts} aria-label={t('split.by_count')} />
					{:else if m === 'size' && mode === 'size'}
						<Input type="number" min="1" class="h-8 w-20" bind:value={max} aria-label={t('split.by_size')} />
					{/if}
				</label>
			{/each}
		</RadioGroup.Root>
		{#if mode !== 'artist'}
			<div>
				<p class="mb-1 text-xs font-medium text-muted-foreground">{t('split.order')}</p>
				<RadioGroup.Root value={order} onValueChange={(v) => (order = v as OrderKey)} class="flex flex-wrap gap-x-4 gap-y-1">
					{#each ORDERS as o (o)}
						<label class="flex cursor-pointer items-center gap-2 text-sm">
							<RadioGroup.Item value={o} />
							{t(`split.order_${o}`)}
						</label>
					{/each}
				</RadioGroup.Root>
				{#if order === 'shuffle'}
					<p class="mt-1 text-xs text-muted-foreground">{t('split.seed', { seed })}</p>
				{/if}
			</div>
		{/if}
		<div class="flex justify-end">
			<Button onclick={preview} disabled={planning} class="gap-2">
				<HugeiconsIcon icon={GitForkIcon} class="h-4 w-4" />
				{planning ? t('common.loading') : t('split.preview')}
			</Button>
		</div>
	</div>
{:else}
	<div class="flex min-h-0 flex-col gap-3">
		<div class="flex items-center gap-2">
			<Button variant="ghost" size="icon-sm" onclick={() => (plan = null)} aria-label={t('common.back')}>
				<HugeiconsIcon icon={ArrowLeft01Icon} class="h-4 w-4" />
			</Button>
			<p class="text-sm" role="status">
				{t('split.summary', { count: plan.parts.length, tracks: plan.rows.length })}
			</p>
		</div>
		<div class="max-h-[45vh] space-y-1 overflow-y-auto pr-1">
			{#each plan.parts as part, i (i)}
				<div class="flex items-center gap-3 rounded-md px-1 py-1 {include[i] ? '' : 'opacity-50'}">
					<Checkbox
						checked={include[i]}
						onCheckedChange={(v) => (include[i] = !!v)}
						aria-label={t('split.include')}
					/>
					<Input class="h-8 flex-1" bind:value={names[i]} aria-label={t('split.name')} />
					<span class="w-24 shrink-0 text-right text-xs text-muted-foreground tabular-nums">
						{part.rows.length === 1 ? t('library.songs_count_one') : t('library.songs_count', { count: part.rows.length })}
					</span>
				</div>
			{/each}
		</div>
		{#if signedIn && !api.isLocalPlaylist(playlistId)}
			<label class="flex items-center gap-2 text-sm">
				<Switch bind:checked={local} />
				{t('build.keep_local')}
			</label>
		{/if}
		<div class="flex justify-end">
			<Button onclick={create} disabled={!chosen.length}>
				{chosen.length === 1 ? t('split.create_one') : t('split.create', { count: chosen.length })}
			</Button>
		</div>
	</div>
{/if}
