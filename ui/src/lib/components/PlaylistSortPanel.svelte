<script lang="ts">
	// The playlist's sort choices, and "Save this order to the playlist" under them once the choice
	// differs from the playlist's own order. The playlist page shows it in its Sort menu (and owns
	// what a choice does: YouTube's sort, the cache, the stored choice); Tools ▸ Reorder shows it in
	// the tools dialog. Presentational: the caller decides when saving is offered and what it does.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Tick02Icon } from '@hugeicons/core-free-icons';
	import * as RadioGroup from '$lib/components/ui/radio-group';
	import { SORTS, type SortKey } from '$lib/sort';
	import { t } from '$lib/i18n.svelte';

	let {
		value,
		onchoose,
		canKeep,
		onkeep,
		saving = false,
		keys = SORTS
	}: {
		value: SortKey;
		onchoose: (key: SortKey) => void;
		/** Offer "Save this order to the playlist". */
		canKeep: boolean;
		onkeep: () => void;
		saving?: boolean;
		keys?: SortKey[];
	} = $props();
</script>

<RadioGroup.Root
	{value}
	onValueChange={(v) => onchoose(v as SortKey)}
	class="gap-0"
>
	{#each keys as key (key)}
		<label
			class="flex w-full cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent/10"
		>
			<RadioGroup.Item value={key} />
			{t(`sort.${key}`)}
		</label>
	{/each}
</RadioGroup.Root>
{#if canKeep}
	<div class="my-1 h-px bg-border"></div>
	<button
		class="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent/10 disabled:opacity-50"
		onclick={onkeep}
		disabled={saving}
	>
		<HugeiconsIcon icon={Tick02Icon} class="h-4 w-4" />
		{t('reorder.keep_sort')}
	</button>
{/if}
