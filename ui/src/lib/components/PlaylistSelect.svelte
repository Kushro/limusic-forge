<script lang="ts">
	// A playlist dropdown with a search field. A library easily runs past fifty playlists, and a
	// plain select makes you scroll for the one you want; here you type a few letters instead.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { Search01Icon, Tick02Icon, UnfoldMoreIcon } from '@hugeicons/core-free-icons';
	import * as Popover from '$lib/components/ui/popover';
	import { Input } from '$lib/components/ui/input';
	import { t } from '$lib/i18n.svelte';
	import { cn } from '$lib/utils.js';

	let {
		value,
		options,
		onpick,
		placeholder,
		label,
		class: className
	}: {
		value: string;
		options: { value: string; label: string }[];
		onpick: (value: string) => void;
		/** Shown on the button while nothing is picked. */
		placeholder: string;
		label: string;
		class?: string;
	} = $props();

	let open = $state(false);
	let query = $state('');
	let list = $state<HTMLDivElement | null>(null);

	const current = $derived(options.find((o) => o.value === value));
	const matches = $derived.by(() => {
		const q = query.trim().toLowerCase();
		return q ? options.filter((o) => o.label.toLowerCase().includes(q)) : options;
	});

	// Each opening starts with an empty search and the current pick in view.
	$effect(() => {
		if (!open) return;
		query = '';
		requestAnimationFrame(() => list?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' }));
	});

	function pick(v: string) {
		open = false;
		if (v !== value) onpick(v);
	}

	function onkeydown(e: KeyboardEvent) {
		if (e.key === 'Enter' && matches.length) {
			e.preventDefault();
			pick(matches[0].value);
		}
	}
</script>

<Popover.Root bind:open>
	<Popover.Trigger
		aria-label={label}
		class={cn(
			'border-input bg-input/30 dark:hover:bg-input/50 focus-visible:border-ring focus-visible:ring-ring/50 flex h-9 items-center justify-between gap-1.5 rounded-4xl border px-3 py-2 text-sm whitespace-nowrap outline-none transition-colors focus-visible:ring-[3px]',
			className
		)}
	>
		<span class="min-w-0 flex-1 truncate text-left {current ? '' : 'text-muted-foreground'}">
			{current?.label ?? placeholder}
		</span>
		<HugeiconsIcon icon={UnfoldMoreIcon} strokeWidth={2} class="pointer-events-none size-4 shrink-0 text-muted-foreground" />
	</Popover.Trigger>
	<Popover.Content align="start" class="w-(--bits-popover-anchor-width) min-w-64 gap-2 p-2">
		<div class="relative">
			<HugeiconsIcon
				icon={Search01Icon}
				strokeWidth={2}
				class="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground"
			/>
			<Input
				bind:value={query}
				{onkeydown}
				class="h-9 pl-9"
				placeholder={t('drop.search')}
				aria-label={t('drop.search')}
			/>
		</div>
		<div bind:this={list} class="max-h-72 overflow-y-auto" role="listbox" aria-label={label}>
			{#each matches as o (o.value)}
				{@const active = o.value === value}
				<button
					type="button"
					role="option"
					aria-selected={active}
					onclick={() => pick(o.value)}
					class="flex w-full items-center gap-2 rounded-xl px-2.5 py-1.5 text-left text-sm hover:bg-accent/10 focus-visible:bg-accent/10 focus-visible:outline-none"
				>
					<span class="min-w-0 flex-1 truncate">{o.label}</span>
					{#if active}
						<HugeiconsIcon icon={Tick02Icon} strokeWidth={2} class="size-4 shrink-0 text-primary" />
					{/if}
				</button>
			{:else}
				<p class="px-2 py-4 text-center text-xs text-muted-foreground">{t('common.no_matches')}</p>
			{/each}
		</div>
	</Popover.Content>
</Popover.Root>
