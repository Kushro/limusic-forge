<script lang="ts">
	// Today's Data API quota as one stacked bar (PlaylistForge's budget gauge): what backups, jobs and
	// everything else spent, the backup reserve still kept, the safety margin and what jobs may still
	// spend, with a legend and the countdown to the reset. The split comes from `budget_partition`.
	import { onMount } from 'svelte';
	import type { BudgetPartition } from '$lib/api';
	import { t } from '$lib/i18n.svelte';
	import { budgetSegments, untilReset, type SegmentKey } from './jobs';

	let { partition }: { partition: BudgetPartition } = $props();

	const COLOR: Record<SegmentKey, string> = {
		backup: 'bg-sky-500',
		jobs: 'bg-indigo-500',
		other: 'bg-slate-500',
		reserve: 'bg-amber-500',
		margin: 'bg-rose-400',
		available: 'bg-emerald-500'
	};
	const LABEL = {
		backup: 'jobs.budget.legend.backup',
		jobs: 'jobs.budget.legend.jobs',
		other: 'jobs.budget.legend.other',
		reserve: 'jobs.budget.legend.reserve',
		margin: 'jobs.budget.legend.margin',
		available: 'jobs.budget.legend.available'
	} as const;

	// The countdown moves once a minute; nothing else here does.
	let now = $state(Date.now());
	onMount(() => {
		const timer = setInterval(() => (now = Date.now()), 60_000);
		return () => clearInterval(timer);
	});

	const segments = $derived(budgetSegments(partition));
	const reset = $derived(untilReset(partition.next_reset, now));
	const num = (n: number) => n.toLocaleString();
</script>

<div class="space-y-3">
	<div class="flex flex-wrap items-baseline justify-between gap-2 text-sm">
		<span class="font-medium tabular-nums">
			{t('jobs.budget.of_daily', { spent: num(partition.spent_total), daily: num(partition.daily_units) })}
		</span>
		<span class="text-muted-foreground tabular-nums">
			{t('jobs.budget.available', { units: num(partition.available_for_jobs) })}
		</span>
	</div>
	<div
		class="flex h-3 w-full overflow-hidden rounded-full bg-muted"
		role="img"
		aria-label={segments.map((s) => t('jobs.budget.units', { label: t(LABEL[s.key]), units: num(s.units) })).join(', ')}
	>
		{#each segments as s (s.key)}
			{#if s.percent > 0}
				<div
					class="h-full {COLOR[s.key]}"
					style="width: {s.percent}%"
					title={t('jobs.budget.units', { label: t(LABEL[s.key]), units: num(s.units) })}
				></div>
			{/if}
		{/each}
	</div>
	<div class="grid grid-cols-2 gap-x-4 gap-y-1.5 text-xs text-muted-foreground sm:grid-cols-3">
		{#each segments as s (s.key)}
			<span class="flex min-w-0 items-center gap-1.5">
				<span class="h-2 w-2 shrink-0 rounded-full {COLOR[s.key]}"></span>
				<span class="truncate tabular-nums">{t('jobs.budget.units', { label: t(LABEL[s.key]), units: num(s.units) })}</span>
			</span>
		{/each}
	</div>
	<div class="border-t pt-2 text-xs text-muted-foreground tabular-nums">
		{t('jobs.budget.resets_in', reset)}
	</div>
</div>
