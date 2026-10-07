<script lang="ts">
	// Data API units spent per Pacific day over the last two weeks, one bar a day, today last. Plain
	// flex boxes like the monitor's alerts chart: no chart library. A dashed line marks the daily
	// quota when some day came near it.
	import type { DailyUsage } from '$lib/api';
	import { t } from '$lib/i18n.svelte';
	import { dayOfMonth, usageBars } from './jobs';

	let { history, daily }: { history: DailyUsage[]; daily: number } = $props();

	const bars = $derived(usageBars(history, daily));
	const total = $derived(history.reduce((n, d) => n + d.units, 0));
	const peak = $derived(Math.max(0, ...history.map((d) => d.units)));
	// The quota line sits at the top of the chart whenever it is the scale (no day went over it).
	const showQuota = $derived(daily > 0 && peak >= daily * 0.5);
	const quotaAt = $derived(peak > daily ? (daily / peak) * 100 : 100);
	const dayTitle = (b: (typeof bars)[number]) =>
		t('jobs.usage.day', {
			day: new Date(`${b.date}T12:00:00`).toLocaleDateString(undefined, { dateStyle: 'medium' }),
			units: b.units.toLocaleString(),
			percent: Math.round(b.ofQuota * 100)
		});
</script>

{#if !total}
	<p class="text-sm text-muted-foreground">{t('jobs.usage.empty')}</p>
{:else}
	<div class="relative flex h-28 items-end gap-1" role="img" aria-label={t('jobs.usage.title')}>
		{#if showQuota}
			<div
				class="pointer-events-none absolute inset-x-0 border-t border-dashed border-muted-foreground/40"
				style="bottom: {quotaAt}%"
			></div>
		{/if}
		{#each bars as b (b.date)}
			<div class="flex h-full min-w-0 flex-1 flex-col justify-end" title={dayTitle(b)}>
				<div
					class="rounded-sm {b.ofQuota >= 1 ? 'bg-rose-500' : b.today ? 'bg-primary' : 'bg-primary/50'}"
					style="height: {b.percent}%"
				></div>
			</div>
		{/each}
	</div>
	<div class="mt-1 flex gap-1">
		{#each bars as b (b.date)}
			<span class="min-w-0 flex-1 text-center text-[10px] tabular-nums text-muted-foreground">{dayOfMonth(b.date)}</span>
		{/each}
	</div>
{/if}
