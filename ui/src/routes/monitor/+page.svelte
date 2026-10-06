<script lang="ts">
	// The playlist monitor at a glance (PlaylistForge's Monitor screen): what the index holds, the
	// last full check's "+N −N ~N", a "Check now" that runs one with its progress, the alerts of the
	// last two weeks as stacked bars per day, the recent runs, and how often checks run by themselves.
	import { onMount, untrack } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		FolderOpenIcon,
		Notification03Icon,
		Radar01Icon,
		RefreshIcon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { AlertKind, AlertStamp, MonitorRun, MonitorStats } from '$lib/api';
	import { ALERT_KINDS, localDay } from '$lib/alerts';
	import { auth, monitor, syncAllPlaylists, toast } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import * as Select from '$lib/components/ui/select';
	import ErrorState from '$lib/components/ErrorState.svelte';

	const CHART_DAYS = 14;
	// The choices the backend accepts (commands.rs MONITOR_INTERVALS); anything else reads as 6.
	const INTERVALS = ['0', '1', '3', '6', '12', '24'];
	const INTERVAL_DEFAULT = '6';

	let stats = $state<MonitorStats | null>(null);
	let runs = $state.raw<MonitorRun[]>([]);
	let stamps = $state.raw<AlertStamp[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let interval = $state(INTERVAL_DEFAULT);

	async function load() {
		try {
			[stats, runs, stamps] = await Promise.all([
				api.monitorStats(),
				api.monitorRuns(20),
				api.alertsByDay(CHART_DAYS)
			]);
			error = null;
		} catch (e) {
			error = String(e);
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		load();
		api
			.getSettings()
			.then((s) => {
				const h = s.monitor_interval_hours?.trim();
				interval = h && INTERVALS.includes(h) ? h : INTERVAL_DEFAULT;
			})
			.catch(() => {});
		// A run finished (any trigger): new stats, a new row, maybe new alerts.
		const off = api.onPlaylistIndexSynced(() => load());
		return () => void off.then((f) => f());
	});

	// A run that failed files a row but no `playlist-index-synced`; its progress ending is the cue.
	let wasRunning = false;
	$effect(() => {
		const running = monitor.progress !== null;
		if (wasRunning && !running) untrack(load);
		wasRunning = running;
	});

	// --- check now ----------------------------------------------------------------------------
	let starting = $state(false);
	const busy = $derived(starting || monitor.progress !== null);
	const signedIn = $derived(!!auth.account?.signedIn);

	async function checkNow() {
		if (busy) return;
		starting = true;
		try {
			await syncAllPlaylists();
		} catch (e) {
			if (String(e) === 'busy') toast(t('monitor.busy'));
			else toast.error(String(e));
		} finally {
			starting = false;
			load();
		}
	}
	const progressPct = $derived(
		monitor.progress && monitor.progress.total > 0
			? Math.round((monitor.progress.done / monitor.progress.total) * 100)
			: 0
	);

	// --- interval -------------------------------------------------------------------------------
	const intervalLabel = (h: string) =>
		h === '0'
			? t('monitor.interval_off')
			: h === '1'
				? t('monitor.interval_one')
				: t('monitor.interval_hours', { count: Number(h) });

	async function chooseInterval(h: string) {
		const before = interval;
		interval = h;
		try {
			await api.setSetting('monitor_interval_hours', h);
		} catch (e) {
			interval = before;
			toast.error(String(e));
		}
	}

	// --- chart ----------------------------------------------------------------------------------
	const KIND_COLOR: Record<AlertKind, string> = {
		added: 'bg-emerald-500',
		removed: 'bg-rose-500',
		moved: 'bg-sky-500',
		unavailable: 'bg-amber-500',
		restored: 'bg-violet-500'
	};
	const kindLabel = (k: AlertKind) => t(`everywhere.kind_${k}`);

	type DayBar = { day: string; date: Date; total: number; counts: Record<AlertKind, number> };
	const bars = $derived.by(() => {
		const now = new Date();
		const out: DayBar[] = [];
		const byDay = new Map<string, DayBar>();
		// Calendar days, oldest first; built from the date so a DST change can't skip or repeat one.
		for (let i = CHART_DAYS - 1; i >= 0; i--) {
			const date = new Date(now.getFullYear(), now.getMonth(), now.getDate() - i, 12);
			const day = localDay(date.getTime() / 1000);
			const bar: DayBar = {
				day,
				date,
				total: 0,
				counts: { added: 0, removed: 0, moved: 0, unavailable: 0, restored: 0 }
			};
			out.push(bar);
			byDay.set(day, bar);
		}
		for (const s of stamps) {
			const bar = byDay.get(localDay(s.at));
			if (!bar || !(s.kind in bar.counts)) continue;
			bar.counts[s.kind]++;
			bar.total++;
		}
		return out;
	});
	const maxBar = $derived(Math.max(1, ...bars.map((b) => b.total)));
	const chartTotal = $derived(bars.reduce((n, b) => n + b.total, 0));
	const chartKinds = $derived(ALERT_KINDS.filter((k) => bars.some((b) => b.counts[k] > 0)));
	const barTitle = (b: DayBar) =>
		[
			t('monitor.chart_day', {
				day: b.date.toLocaleDateString(undefined, { dateStyle: 'medium' }),
				count: b.total
			}),
			...ALERT_KINDS.filter((k) => b.counts[k]).map((k) => `${kindLabel(k)}: ${b.counts[k]}`)
		].join('\n');

	// --- formatting -----------------------------------------------------------------------------
	const num = (n: number) => n.toLocaleString();
	const whenOf = (at: number) =>
		new Date(at * 1000).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
	function duration(secs: number) {
		const s = Math.max(0, Math.round(secs));
		if (s < 60) return `${s}s`;
		const m = Math.floor(s / 60);
		if (m < 60) return `${m}m ${String(s % 60).padStart(2, '0')}s`;
		return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, '0')}m`;
	}
	const outcomeVariant = (o: MonitorRun['outcome']) =>
		o === 'ok' ? ('label' as const) : o === 'failed' ? ('outline' as const) : ('muted' as const);

	const openBackups = () => api.openBackupsDir().catch((e) => toast.error(String(e)));

	const statCards = $derived(
		stats
			? [
					{ label: t('monitor.stat_playlists'), value: stats.playlists, hint: '' },
					{ label: t('monitor.stat_items'), value: stats.items, hint: '' },
					{ label: t('monitor.stat_unavailable'), value: stats.unavailable, hint: '' },
					{
						label: t('monitor.stat_duplicates'),
						value: stats.duplicates_estimate,
						hint: t('monitor.stat_duplicates_hint')
					}
				]
			: []
	);
</script>

<div class="p-6">
	<div class="mb-4 flex flex-wrap items-start gap-3">
		<div class="min-w-0 flex-1">
			<h1 class="flex items-center gap-2 font-heading text-2xl font-bold tracking-tight">
				<HugeiconsIcon icon={Radar01Icon} class="h-6 w-6 text-primary" />
				{t('monitor.title')}
			</h1>
			<p class="mt-1 text-sm text-muted-foreground">{t('monitor.intro')}</p>
		</div>
		<div class="flex flex-wrap items-center gap-2">
			<Button variant="ghost" size="sm" class="gap-1.5" href="/alerts">
				<HugeiconsIcon icon={Notification03Icon} class="h-4 w-4" />
				{t('monitor.open_alerts')}
				{#if monitor.unseen > 0}
					<Badge variant="chip">{monitor.unseen > 99 ? '99+' : monitor.unseen}</Badge>
				{/if}
			</Button>
			<Button variant="ghost" size="sm" class="gap-1.5" onclick={openBackups}>
				<HugeiconsIcon icon={FolderOpenIcon} class="h-4 w-4" />
				{t('monitor.open_backups')}
			</Button>
		</div>
	</div>

	{#if loading}
		<div class="mb-4 grid grid-cols-2 gap-3 md:grid-cols-4">
			{#each Array(4) as _, i (i)}
				<div class="h-20 animate-pulse rounded-xl bg-card/40"></div>
			{/each}
		</div>
		<div class="h-40 animate-pulse rounded-xl bg-card/40"></div>
	{:else if error}
		<ErrorState message={error} onRetry={load} />
	{:else}
		<!-- Stats -->
		<div class="mb-4 grid grid-cols-2 gap-3 md:grid-cols-4">
			{#each statCards as c (c.label)}
				<div class="rounded-xl border px-4 py-3" title={c.hint || undefined}>
					<div class="text-xs font-medium uppercase tracking-wide text-muted-foreground">{c.label}</div>
					<div class="mt-1 text-2xl font-semibold tabular-nums">{num(c.value)}</div>
				</div>
			{/each}
		</div>

		<!-- Last check, Check now, interval -->
		<section class="mb-4 rounded-xl border px-4 py-3">
			<div class="flex flex-wrap items-center gap-3">
				<div class="min-w-0 flex-1">
					<div class="text-xs font-medium uppercase tracking-wide text-muted-foreground">
						{t('monitor.last_check')}
					</div>
					{#if monitor.summary}
						{@const s = monitor.summary}
						<div class="mt-1 flex flex-wrap items-baseline gap-x-3 gap-y-1 text-sm">
							<span class="tabular-nums" title={`${kindLabel('added')} · ${kindLabel('removed')} · ${kindLabel('moved')}`}>
								<span class="font-semibold text-emerald-600 dark:text-emerald-400">+{s.added}</span>
								<span class="font-semibold text-rose-600 dark:text-rose-400">−{s.removed}</span>
								<span class="font-semibold text-sky-600 dark:text-sky-400">~{s.moved}</span>
							</span>
							{#if s.unavailable}
								<span class="tabular-nums text-muted-foreground">{s.unavailable} {kindLabel('unavailable')}</span>
							{/if}
							{#if s.restored}
								<span class="tabular-nums text-muted-foreground">{s.restored} {kindLabel('restored')}</span>
							{/if}
							<span class="text-muted-foreground">{t('monitor.summary_alerts', { count: s.alerts_new })}</span>
							{#if s.failed}
								<span class="text-destructive">{t('monitor.summary_failed', { count: s.failed })}</span>
							{/if}
						</div>
						<div class="mt-0.5 text-xs text-muted-foreground">
							{t('monitor.summary_when', { when: whenOf(s.at), count: s.playlists })}
						</div>
					{:else}
						<div class="mt-1 text-sm text-muted-foreground">{t('monitor.never')}</div>
					{/if}
				</div>
				<Button
					class="gap-1.5"
					disabled={busy || !signedIn}
					title={signedIn ? undefined : t('monitor.signed_out')}
					onclick={checkNow}
				>
					<HugeiconsIcon icon={RefreshIcon} class="h-4 w-4 {busy ? 'animate-spin' : ''}" />
					{busy ? t('monitor.checking') : t('monitor.check_now')}
				</Button>
			</div>
			{#if monitor.progress}
				{@const p = monitor.progress}
				<div class="mt-3" role="status">
					<div
						class="h-1.5 overflow-hidden rounded-full bg-muted"
						role="progressbar"
						aria-valuemin={0}
						aria-valuemax={p.total}
						aria-valuenow={p.done}
					>
						<div class="h-full rounded-full bg-primary transition-[width]" style="width: {progressPct}%"></div>
					</div>
					<div class="mt-1 flex gap-2 text-xs text-muted-foreground">
						<span class="min-w-0 flex-1 truncate">{p.current ?? ''}</span>
						<span class="shrink-0 tabular-nums">{t('monitor.progress', { done: p.done, total: p.total })}</span>
					</div>
				</div>
			{:else if !signedIn}
				<p class="mt-2 text-xs text-muted-foreground">{t('monitor.signed_out')}</p>
			{/if}
			<div class="mt-3 flex flex-wrap items-center gap-3 border-t pt-3">
				<span class="text-sm">{t('monitor.interval')}</span>
				<Select.Root type="single" value={interval} onValueChange={(v) => v && v !== interval && chooseInterval(v)}>
					<Select.Trigger class="w-48" aria-label={t('monitor.interval')}>
						<span class="flex-1 truncate text-left">{intervalLabel(interval)}</span>
					</Select.Trigger>
					<Select.Content>
						{#each INTERVALS as h (h)}
							<Select.Item value={h} label={intervalLabel(h)}>{intervalLabel(h)}</Select.Item>
						{/each}
					</Select.Content>
				</Select.Root>
				<span class="text-xs text-muted-foreground">{t('monitor.interval_hint')}</span>
			</div>
		</section>

		<!-- Alerts per day, stacked by kind -->
		<section class="mb-4 rounded-xl border px-4 py-3">
			<h2 class="mb-3 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
				{t('monitor.chart_title')}
			</h2>
			{#if !chartTotal}
				<p class="text-sm text-muted-foreground">{t('monitor.chart_empty')}</p>
			{:else}
				<div class="flex h-32 items-end gap-1" role="img" aria-label={t('monitor.chart_title')}>
					{#each bars as b (b.day)}
						<div class="flex h-full min-w-0 flex-1 flex-col justify-end" title={barTitle(b)}>
							<div class="flex flex-col-reverse overflow-hidden rounded-sm" style="height: {(b.total / maxBar) * 100}%">
								{#each ALERT_KINDS as k (k)}
									{#if b.counts[k]}
										<div class={KIND_COLOR[k]} style="height: {(b.counts[k] / b.total) * 100}%"></div>
									{/if}
								{/each}
							</div>
						</div>
					{/each}
				</div>
				<div class="mt-1 flex gap-1">
					{#each bars as b (b.day)}
						<span class="min-w-0 flex-1 text-center text-[10px] tabular-nums text-muted-foreground">{b.date.getDate()}</span>
					{/each}
				</div>
				<div class="mt-2 flex flex-wrap gap-3 text-xs">
					{#each chartKinds as k (k)}
						<span class="flex items-center gap-1.5">
							<span class="h-2.5 w-2.5 rounded-sm {KIND_COLOR[k]}"></span>
							{kindLabel(k)}
						</span>
					{/each}
				</div>
			{/if}
		</section>

		<!-- Run history -->
		<section class="rounded-xl border">
			<h2 class="px-4 pt-3 pb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
				{t('monitor.runs_title')}
			</h2>
			{#if !runs.length}
				<p class="px-4 pb-3 text-sm text-muted-foreground">{t('monitor.runs_empty')}</p>
			{:else}
				<div class="overflow-x-auto">
					<table class="w-full text-sm">
						<thead class="text-left text-xs text-muted-foreground">
							<tr class="border-b">
								<th class="px-4 py-2 font-medium">{t('monitor.col_started')}</th>
								<th class="px-2 py-2 font-medium">{t('monitor.col_duration')}</th>
								<th class="px-2 py-2 font-medium">{t('monitor.col_trigger')}</th>
								<th class="px-2 py-2 font-medium">{t('monitor.col_outcome')}</th>
								<th class="px-2 py-2 text-right font-medium">{t('monitor.col_playlists')}</th>
								<th class="px-4 py-2 text-right font-medium">{t('monitor.col_alerts')}</th>
							</tr>
						</thead>
						<tbody>
							{#each runs as r (r.id)}
								<tr class="border-b last:border-b-0 hover:bg-accent/10">
									<td class="px-4 py-1.5 whitespace-nowrap">{whenOf(r.started_at)}</td>
									<td class="px-2 py-1.5 tabular-nums whitespace-nowrap">{duration(r.finished_at - r.started_at)}</td>
									<td class="px-2 py-1.5 whitespace-nowrap">{t(`monitor.trigger_${r.trigger}`)}</td>
									<td class="px-2 py-1.5">
										<Badge variant={outcomeVariant(r.outcome)}>{t(`monitor.outcome_${r.outcome}`)}</Badge>
									</td>
									<td class="px-2 py-1.5 text-right tabular-nums whitespace-nowrap">
										{r.playlists_ok}
										<span class="text-muted-foreground">/</span>
										<span class={r.playlists_failed ? 'text-destructive' : 'text-muted-foreground'}>{r.playlists_failed}</span>
									</td>
									<td class="px-4 py-1.5 text-right tabular-nums">{r.alerts_new}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</section>
	{/if}
</div>
