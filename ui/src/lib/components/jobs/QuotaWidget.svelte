<script lang="ts">
	// Today's YouTube Data API spend: used of the daily quota, the countdown to the reset (midnight
	// Pacific) and, in full, the spend by call. Shown only once the Data API is set up (any state
	// past `not_configured`): with nothing connected there is no quota to watch. Keeps itself current
	// from `quota-changed`. `compact` is the monitor page's one-line card, linking to the jobs page.
	import { onMount } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { ArrowRight01Icon } from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { QuotaToday } from '$lib/api';
	import { t } from '$lib/i18n.svelte';
	import { trackYtData, ytdata } from '$lib/ytdata.svelte';
	import { spentFraction, untilReset } from './jobs';

	let { compact = false }: { compact?: boolean } = $props();

	let quota = $state<QuotaToday | null>(null);
	let now = $state(Date.now());

	const connected = $derived(!!ytdata.status && ytdata.status.state !== 'not_configured');

	async function load() {
		try {
			quota = await api.quotaToday();
			now = Date.now();
		} catch {
			// Outside Tauri, or the database is busy: the next change reloads it.
		}
	}

	onMount(() => {
		trackYtData();
		void load();
		const off = api.onQuotaChanged(() => void load());
		const timer = setInterval(() => (now = Date.now()), 60_000);
		return () => {
			clearInterval(timer);
			void off.then((f) => f());
		};
	});

	const fraction = $derived(quota ? spentFraction(quota.spent, quota.daily_units) : 0);
	const reset = $derived(quota ? untilReset(quota.next_reset, now) : { hours: 0, minutes: 0 });
	const tone = $derived(fraction >= 0.97 ? 'bg-rose-500' : fraction >= 0.8 ? 'bg-amber-500' : 'bg-primary');
	const num = (n: number) => n.toLocaleString();
</script>

{#if connected && quota}
	{#if compact}
		<a
			href="/jobs"
			class="mb-4 flex flex-wrap items-center gap-3 rounded-xl border px-4 py-3 transition-colors hover:bg-accent/10"
		>
			<div class="min-w-0 flex-1">
				<div class="text-xs font-medium uppercase tracking-wide text-muted-foreground">{t('jobs.quota.title')}</div>
				<div class="mt-1 h-1.5 overflow-hidden rounded-full bg-muted">
					<div class="h-full rounded-full {tone}" style="width: {fraction * 100}%"></div>
				</div>
			</div>
			<span class="text-sm font-medium tabular-nums">
				{t('jobs.quota.spent', { spent: num(quota.spent), daily: num(quota.daily_units) })}
			</span>
			<span class="text-xs text-muted-foreground tabular-nums">{t('jobs.quota.resets_in', reset)}</span>
			<span class="flex items-center gap-1 text-xs text-primary">
				{t('jobs.quota.open_jobs')}
				<HugeiconsIcon icon={ArrowRight01Icon} class="h-3.5 w-3.5" />
			</span>
		</a>
	{:else}
		<div class="space-y-3">
			<div class="flex flex-wrap items-baseline justify-between gap-2">
				<span class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{t('jobs.quota.title')}</span>
				<span class="text-sm font-medium tabular-nums">
					{t('jobs.quota.spent', { spent: num(quota.spent), daily: num(quota.daily_units) })}
				</span>
			</div>
			<div
				class="h-2 overflow-hidden rounded-full bg-muted"
				role="progressbar"
				aria-valuemin={0}
				aria-valuemax={quota.daily_units}
				aria-valuenow={quota.spent}
				aria-label={t('jobs.quota.title')}
			>
				<div class="h-full rounded-full {tone}" style="width: {fraction * 100}%"></div>
			</div>
			<div class="text-xs text-muted-foreground tabular-nums">{t('jobs.quota.resets_in', reset)}</div>
			<div>
				<div class="mb-1 text-xs font-medium text-muted-foreground">{t('jobs.quota.by_endpoint')}</div>
				{#if !quota.endpoints.length}
					<p class="text-xs text-muted-foreground">{t('jobs.quota.none_today')}</p>
				{:else}
					<ul class="space-y-0.5 text-xs">
						{#each quota.endpoints as e (e.endpoint)}
							<li class="flex items-center gap-2">
								<span class="min-w-0 flex-1 truncate font-mono">{e.endpoint}</span>
								<span class="text-muted-foreground tabular-nums">{t('jobs.quota.calls', { count: num(e.calls) })}</span>
								<span class="w-16 text-right font-medium tabular-nums">{num(e.units)} u</span>
							</li>
						{/each}
					</ul>
				{/if}
			</div>
		</div>
	{/if}
{/if}
