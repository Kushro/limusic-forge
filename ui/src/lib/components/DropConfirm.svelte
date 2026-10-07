<script lang="ts">
	// What a drop on a sidebar playlist should do, asked next to the row it landed on: copy or move,
	// and what to do with tracks the playlist already has. PlaylistForge's drop confirmation, as a
	// popover instead of a bottom sheet, since the target is right there under the pointer.
	// Below, the engine for this drop and what it costs on the Data API (`EngineChoice`).
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Copy01Icon, SquareArrowRightDoubleIcon } from '@hugeicons/core-free-icons';
	import { isLocalPlaylist, withEngineChoice, type BrowseItem } from '$lib/api';
	import type { PlaylistEngine } from '$lib/ytdata.svelte';
	import EngineChoice from './ytdata/EngineChoice.svelte';
	import type { TrackRowsDrag } from '$lib/dnd';
	import { fitMenu, type Anchor } from '$lib/menu';
	import { prefs, type DropDupes } from '$lib/player.svelte';
	import { rememberDrop, transfer } from '$lib/transfer.svelte';
	import { t } from '$lib/i18n.svelte';
	import { Button } from './ui/button';
	import { Checkbox } from './ui/checkbox';
	import * as RadioGroup from './ui/radio-group';

	let {
		drag,
		target,
		anchor,
		onclose
	}: { drag: TrackRowsDrag; target: BrowseItem; anchor: Anchor; onclose: () => void } = $props();

	// Consolidate only means something for a move; a copy-only drop falls back to Skip. Read once:
	// the popover is made for one drop and closed after it.
	const initialDupes = (): DropDupes =>
		prefs.dropDupes === 'consolidate' && !drag.from ? 'skip' : prefs.dropDupes;
	let dupes = $state<DropDupes>(initialDupes());
	let remember = $state(false);
	const count = $derived(drag.rows.length);
	const canMove = $derived(!!drag.from);
	const title = $derived(
		count === 1 ? t('drop.title_one', { playlist: target.title }) : t('drop.title', { count, playlist: target.title })
	);
	const choices = $derived<DropDupes[]>(canMove ? ['skip', 'allow', 'consolidate'] : ['skip', 'allow']);
	/** This drop's engine, once the user picks one; unset follows `playlist_engine`. */
	let engine = $state<PlaylistEngine | undefined>();
	const priced = $derived([
		{ label: t('drop.copy'), kind: 'copy' as const, rows: count, playlists: [target.id] },
		...(drag.from
			? [{ label: t('drop.move'), kind: 'move' as const, rows: count, playlists: [target.id, drag.from] }]
			: [])
	]);

	async function go(mode: 'copy' | 'move') {
		if (remember) rememberDrop(mode, dupes);
		// Read before closing: closing unmounts this, and an unmounted component's props are gone.
		const [d, to, policy, choice] = [drag, target, dupes, engine];
		onclose();
		// `transfer` sends its write before its first await, which is all the choice has to cover.
		await withEngineChoice(choice ?? null, () => transfer(d, to, mode, policy));
	}

	function onKey(e: KeyboardEvent) {
		if (e.key === 'Escape') {
			e.stopPropagation();
			onclose();
		}
	}
</script>

<svelte:window onkeydown={onKey} />

<button
	class="fixed inset-0 z-40 cursor-default"
	onclick={onclose}
	aria-label={t('a11y.close_menu')}
></button>
<div
	class="fixed z-50 w-72 animate-in rounded-lg border bg-popover p-3 text-popover-foreground shadow-xl duration-150 fade-in-0 zoom-in-95"
	style={anchor.style}
	role="dialog"
	aria-label={title}
	{@attach fitMenu(anchor)}
>
	<p class="truncate text-sm font-medium" title={target.title}>
		{title}
	</p>
	<div class="mt-3 grid gap-2 {canMove ? 'grid-cols-2' : 'grid-cols-1'}">
		<Button variant="outline" size="sm" class="gap-1.5" onclick={() => go('copy')}>
			<HugeiconsIcon icon={Copy01Icon} class="h-4 w-4" />
			{t('drop.copy')}
		</Button>
		{#if canMove}
			<Button size="sm" class="gap-1.5" onclick={() => go('move')}>
				<HugeiconsIcon icon={SquareArrowRightDoubleIcon} class="h-4 w-4" />
				{t('drop.move')}
			</Button>
		{/if}
	</div>
	<p class="mt-3 text-xs font-medium text-muted-foreground">{t('drop.duplicates')}</p>
	<RadioGroup.Root
		value={dupes}
		onValueChange={(v) => (dupes = v as DropDupes)}
		class="mt-1 gap-0"
	>
		{#each choices as key (key)}
			<label class="flex cursor-pointer items-start gap-2 rounded-md px-1 py-1 text-sm hover:bg-accent/10">
				<RadioGroup.Item value={key} class="mt-0.5" />
				<span>
					{t(`drop.dupes_${key}`)}
					{#if key === 'consolidate'}
						<span class="block text-xs text-muted-foreground">{t('drop.dupes_consolidate_hint')}</span>
					{/if}
				</span>
			</label>
		{/each}
	</RadioGroup.Root>
	<label class="mt-2 flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
		<Checkbox bind:checked={remember} />
		{t('drop.remember')}
	</label>
	{#if !isLocalPlaylist(target.id)}
		<EngineChoice bind:choice={engine} ops={priced} />
	{/if}
</div>
