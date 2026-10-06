<script lang="ts">
	// The tools hub (PlaylistForge's Operations screen): one card per playlist tool. Pick the
	// playlist to work on, then a card opens that tool in the same dialog a playlist's ⋯ menu opens
	// (`PlaylistToolsDialog`), on that playlist.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		ArrowRight01Icon,
		Copy01Icon,
		FilterIcon,
		GitForkIcon,
		GitMergeIcon,
		SortingAZ01Icon,
		Wrench01Icon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import { library, ownedByUser, personal } from '$lib/player.svelte';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { t } from '$lib/i18n.svelte';
	import * as Select from '$lib/components/ui/select';
	import PlaylistToolsDialog, { type ToolTab } from '$lib/components/PlaylistToolsDialog.svelte';

	type Card = { tab: ToolTab; icon: typeof Wrench01Icon };
	const CARDS: Card[] = [
		{ tab: 'extract', icon: FilterIcon },
		{ tab: 'split', icon: GitForkIcon },
		{ tab: 'duplicates', icon: Copy01Icon },
		{ tab: 'merge', icon: GitMergeIcon },
		{ tab: 'reorder', icon: SortingAZ01Icon }
	];

	// Same list and order as the sidebar. On Repeat is built from play counts: nothing to work on.
	const playlists = $derived(
		orderLibrary(mergeSaved(personal, library.items, 'playlist'), personal).filter(
			(p) => p.kind === 'playlist' && p.id !== api.ON_REPEAT_ID
		)
	);
	let sourceId = $state('');
	const source = $derived(playlists.find((p) => p.id === sourceId) ?? null);
	// What the playlist page calls `reorderable`: yours to change, and not Liked Music.
	const editable = $derived(
		!!source &&
			source.id !== api.LIKED_MUSIC_ID &&
			(api.isLocalPlaylist(source.id) || ownedByUser(source))
	);

	let open = $state(false);
	let tab = $state<ToolTab>('extract');

	function start(which: ToolTab) {
		if (!source) return;
		tab = which;
		open = true;
	}
</script>

<div class="p-6">
	<div class="mb-4">
		<h1 class="flex items-center gap-2 font-heading text-2xl font-bold tracking-tight">
			<HugeiconsIcon icon={Wrench01Icon} class="h-6 w-6 text-primary" />
			{t('tools.hub_title')}
		</h1>
		<p class="mt-1 max-w-2xl text-sm text-muted-foreground">{t('tools.hub_intro')}</p>
	</div>

	<section class="mb-4 flex flex-wrap items-center gap-3 rounded-xl border px-4 py-3">
		<span class="text-sm">{t('tools.source')}</span>
		{#if playlists.length}
			<Select.Root type="single" value={sourceId} onValueChange={(v) => (sourceId = v)}>
				<Select.Trigger class="w-72 max-w-full" aria-label={t('tools.source')}>
					<span class="flex-1 truncate text-left {source ? '' : 'text-muted-foreground'}">
						{source?.title ?? t('tools.source_pick')}
					</span>
				</Select.Trigger>
				<Select.Content class="max-h-80">
					{#each playlists as p (p.id)}
						<Select.Item value={p.id} label={p.title}>{p.title}</Select.Item>
					{/each}
				</Select.Content>
			</Select.Root>
		{:else}
			<span class="text-sm text-muted-foreground">{t('tools.source_none')}</span>
		{/if}
	</section>

	<div class="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
		{#each CARDS as c (c.tab)}
			<div class="flex flex-col gap-3 rounded-xl border p-5 transition-colors hover:border-foreground/20">
				<div class="flex h-10 w-10 items-center justify-center rounded-lg bg-primary/15">
					<HugeiconsIcon icon={c.icon} class="h-5 w-5 text-primary" />
				</div>
				<div class="flex-1">
					<p class="text-sm font-semibold">{t(`tools.card_${c.tab}_title`)}</p>
					<p class="mt-1 text-xs text-muted-foreground">{t(`tools.card_${c.tab}_desc`)}</p>
				</div>
				<button
					type="button"
					class="flex cursor-pointer items-center gap-1.5 self-start text-xs font-medium text-primary hover:underline disabled:cursor-default disabled:opacity-50 disabled:hover:no-underline"
					onclick={() => start(c.tab)}
					disabled={!source}
					title={source ? undefined : t('tools.source_first')}
				>
					{t('tools.start')}
					<HugeiconsIcon icon={ArrowRight01Icon} class="h-3 w-3" />
				</button>
			</div>
		{/each}
	</div>
</div>

{#if source}
	<PlaylistToolsDialog bind:open bind:tab playlistId={source.id} title={source.title} {editable} />
{/if}
