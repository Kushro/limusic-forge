<script lang="ts">
	// "Move to…" from the selection bar: the keyboard-and-click way to do what a drop on a sidebar
	// playlist does, for when the sidebar is collapsed or the target is far down it. One target,
	// unlike Add to playlist: a track can only end up living in one place.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { ComputerIcon, MusicNote01Icon } from '@hugeicons/core-free-icons';
	import * as Dialog from '$lib/components/ui/dialog';
	import * as RadioGroup from './ui/radio-group';
	import { Input } from './ui/input';
	import * as api from '$lib/api';
	import type { BrowseItem } from '$lib/api';
	import type { TrackRowsDrag } from '$lib/dnd';
	import { thumb } from '$lib/thumb';
	import { library, personal, prefs, type DropDupes } from '$lib/player.svelte';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { canDropOn, transfer } from '$lib/transfer.svelte';
	import { t } from '$lib/i18n.svelte';

	let {
		drag = $bindable(),
		ondone
	}: { drag: TrackRowsDrag | null; ondone?: () => void } = $props();

	const DUPES: DropDupes[] = ['skip', 'allow', 'consolidate'];
	let filter = $state('');
	let dupes = $state<DropDupes>(prefs.dropDupes);
	let moving = $state(false);
	// Same list and order as the sidebar, so the playlist you are looking for is where you expect.
	const targets = $derived(
		orderLibrary(mergeSaved(personal, library.items, 'playlist'), personal).filter((p) =>
			canDropOn(p, drag?.from ?? null)
		)
	);
	const matches = $derived(
		targets.filter((p) => p.title.toLowerCase().includes(filter.trim().toLowerCase()))
	);

	$effect(() => {
		if (drag) {
			filter = '';
			dupes = prefs.dropDupes;
		}
	});

	async function pick(target: BrowseItem) {
		if (!drag || moving) return;
		moving = true;
		try {
			if (await transfer(drag, target, 'move', dupes)) {
				drag = null;
				ondone?.();
			}
		} finally {
			moving = false;
		}
	}
</script>

<Dialog.Root open={!!drag} onOpenChange={(v) => !v && !moving && (drag = null)}>
	<Dialog.Content class="sm:max-w-md">
		<Dialog.Header>
			<Dialog.Title>
				{drag?.rows.length === 1
					? t('drop.move_title_one')
					: t('drop.move_title', { count: drag?.rows.length ?? 0 })}
			</Dialog.Title>
			<Dialog.Description>{t('drop.move_desc')}</Dialog.Description>
		</Dialog.Header>
		<Input bind:value={filter} placeholder={t('drop.search')} aria-label={t('drop.search')} />
		<div class="max-h-72 overflow-y-auto">
			{#each matches as p (p.id)}
				<button
					class="flex w-full items-center gap-3 rounded-lg px-2 py-1.5 text-left hover:bg-accent/10 disabled:opacity-50"
					disabled={moving}
					onclick={() => pick(p)}
				>
					<span class="h-10 w-10 shrink-0 overflow-hidden rounded-md bg-muted">
						{#if p.thumbnail}
							<img src={thumb(p.thumbnail, 96)} alt="" class="h-full w-full object-cover" loading="lazy" />
						{:else}
							<span class="flex h-full w-full items-center justify-center text-muted-foreground/50">
								<HugeiconsIcon icon={MusicNote01Icon} class="h-4 w-4" />
							</span>
						{/if}
					</span>
					<span class="min-w-0 flex-1 truncate text-sm font-medium">{p.title}</span>
					{#if api.isLocalPlaylist(p.id)}
						<HugeiconsIcon icon={ComputerIcon} class="h-4 w-4 shrink-0 text-muted-foreground" />
					{/if}
				</button>
			{:else}
				<p class="px-2 py-4 text-sm text-muted-foreground">{t('drop.no_targets')}</p>
			{/each}
		</div>
		<div>
			<p class="text-xs font-medium text-muted-foreground">{t('drop.duplicates')}</p>
			<RadioGroup.Root
				value={dupes}
				onValueChange={(v) => (dupes = v as DropDupes)}
				class="mt-1 flex flex-wrap gap-x-4 gap-y-1"
			>
				{#each DUPES as key (key)}
					<label class="flex cursor-pointer items-center gap-2 text-sm">
						<RadioGroup.Item value={key} />
						{t(`drop.dupes_${key}`)}
					</label>
				{/each}
			</RadioGroup.Root>
		</div>
	</Dialog.Content>
</Dialog.Root>
