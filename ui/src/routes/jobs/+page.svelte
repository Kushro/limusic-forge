<script lang="ts">
	// The jobs page (D30, PlaylistForge's Jobs screen): today's Data API budget as a bar with the quota
	// widget and the last two weeks of use, a summary of the downloads, the queue of playlist jobs
	// with their controls, and the history: ended jobs and the undo journal's edits, each with Undo.
	// Kept current from `jobs-changed`, `quota-changed`, `playlists-edited` and the download events.
	import { onMount } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { FolderOpenIcon, RefreshIcon, TaskDaily01Icon, UndoIcon } from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type {
		BudgetPartition,
		DailyUsage,
		DownloadProgress,
		DownloadRow,
		DownloadStatus,
		JobView,
		PlaylistOp
	} from '$lib/api';
	import { localDay } from '$lib/alerts';
	import { t } from '$lib/i18n.svelte';
	import { toast } from '$lib/player.svelte';
	import { undoOp } from '$lib/playlistops.svelte';
	import { trackYtData, ytdata } from '$lib/ytdata.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import ErrorState from '$lib/components/ErrorState.svelte';
	import BudgetBar from '$lib/components/jobs/BudgetBar.svelte';
	import JobRow from '$lib/components/jobs/JobRow.svelte';
	import QuotaWidget from '$lib/components/jobs/QuotaWidget.svelte';
	import UsageChart from '$lib/components/jobs/UsageChart.svelte';
	import {
		canMove,
		groupByDay,
		mergeHistory,
		moveId,
		queueCounts,
		type HistoryEntry
	} from '$lib/components/jobs/jobs';

	const USAGE_DAYS = 14;
	const DOWNLOADS_READ = 500;

	let active = $state.raw<JobView[]>([]);
	let ended = $state.raw<JobView[]>([]);
	let ops = $state.raw<PlaylistOp[]>([]);
	let partition = $state<BudgetPartition | null>(null);
	let usage = $state.raw<DailyUsage[]>([]);
	let downloads = $state.raw<DownloadRow[]>([]);
	let downloading = $state<DownloadProgress | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);

	const connected = $derived(!!ytdata.status && ytdata.status.state !== 'not_configured');

	async function loadJobs() {
		try {
			[active, ended, ops] = await Promise.all([
				api.jobsList('active'),
				api.jobsList('history'),
				api.playlistHistory()
			]);
			error = null;
		} catch (e) {
			error = String(e);
		} finally {
			loading = false;
		}
	}

	async function loadQuota() {
		try {
			[partition, usage] = await Promise.all([api.budgetPartition(), api.quotaHistory(USAGE_DAYS)]);
		} catch {
			// The bar and the chart stay as they were; the next change reloads them.
		}
	}

	async function loadDownloads() {
		try {
			[downloads, downloading] = await Promise.all([
				api.downloadsRecent(DOWNLOADS_READ),
				api.downloadActive()
			]);
		} catch {
			downloads = [];
		}
	}

	// Several `jobs-changed` arrive per item while a job runs: one reload per burst.
	let pending: ReturnType<typeof setTimeout> | null = null;
	function soon(run: () => void) {
		if (pending) clearTimeout(pending);
		pending = setTimeout(() => {
			pending = null;
			run();
		}, 250);
	}

	onMount(() => {
		trackYtData();
		void loadJobs();
		void loadQuota();
		void loadDownloads();
		const offs = [
			api.onJobsChanged(() => soon(() => void loadJobs())),
			api.onQuotaChanged(() => void loadQuota()),
			api.onPlaylistsEdited(() => soon(() => void loadJobs())),
			api.onDownloadProgress((p) => {
				const finished = downloading !== null && p === null;
				downloading = p;
				if (finished) void loadDownloads();
			})
		];
		return () => {
			if (pending) clearTimeout(pending);
			for (const off of offs) void off.then((f) => f());
		};
	});

	const reloadAll = () => {
		void loadJobs();
		void loadQuota();
	};

	// --- queue ----------------------------------------------------------------------------------
	const counts = $derived(queueCounts(active));
	const summary = $derived(
		[
			counts.active ? t('jobs.summary_active', { count: counts.active }) : '',
			counts.waiting_quota ? t('jobs.summary_waiting_quota', { count: counts.waiting_quota }) : '',
			counts.waiting_auth ? t('jobs.summary_waiting_auth', { count: counts.waiting_auth }) : '',
			counts.paused ? t('jobs.summary_paused', { count: counts.paused }) : ''
		]
			.filter(Boolean)
			.join(' · ')
	);

	async function move(index: number, delta: -1 | 1) {
		const ids = moveId(
			active.map((j) => j.id),
			index,
			index + delta
		);
		// Show the new order at once; the reload confirms it.
		const byId = new Map(active.map((j) => [j.id, j]));
		active = ids.map((id) => byId.get(id)!).filter(Boolean);
		try {
			await api.jobsReorder(ids);
		} catch (e) {
			toast.error(t('jobs.toast.failed', { error: String(e) }));
		}
		void loadJobs();
	}

	// --- history --------------------------------------------------------------------------------
	const history = $derived(groupByDay(mergeHistory(ended, ops), localDay));
	const dayLabel = (day: string) =>
		new Date(`${day}T12:00:00`).toLocaleDateString(undefined, {
			weekday: 'long',
			day: 'numeric',
			month: 'long'
		});
	const timeOf = (at: number) =>
		new Date(at * 1000).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
	const opPlaylists = (op: PlaylistOp) => op.summary.playlists.map((p) => p.title || p.id).join(', ');
	const key = (e: HistoryEntry) => (e.type === 'job' ? `job:${e.job.id}` : `op:${e.op.id}`);

	let undoing = $state<number | null>(null);
	async function undo(op: PlaylistOp) {
		if (undoing !== null) return;
		undoing = op.id;
		try {
			await undoOp(op.id);
		} finally {
			undoing = null;
			void loadJobs();
		}
	}

	// --- downloads ------------------------------------------------------------------------------
	const DOWNLOAD_ORDER: DownloadStatus[] = ['running', 'queued', 'error', 'missing', 'available'];
	const downloadCounts = $derived.by(() => {
		const c: Record<DownloadStatus, number> = { queued: 0, running: 0, available: 0, error: 0, missing: 0 };
		for (const d of downloads) if (d.status in c) c[d.status]++;
		return c;
	});
	const failedDownloads = $derived(downloads.filter((d) => d.status === 'error'));

	let retrying = $state(false);
	async function retryDownloads() {
		if (retrying) return;
		retrying = true;
		let n = 0;
		try {
			for (const d of failedDownloads) if (await api.downloadRetry(d.video_id, d.format)) n++;
			toast.success(t('jobs.downloads.retried', { count: n }));
		} catch (e) {
			toast.error(t('jobs.toast.failed', { error: String(e) }));
		} finally {
			retrying = false;
			void loadDownloads();
		}
	}
	const openFolder = () => api.openDownloadsDir().catch((e) => toast.error(String(e)));
</script>

<div class="p-6">
	<div class="mb-4">
		<h1 class="flex items-center gap-2 font-heading text-2xl font-bold tracking-tight">
			<HugeiconsIcon icon={TaskDaily01Icon} class="h-6 w-6 text-primary" />
			{t('jobs.page_title')}
		</h1>
		<p class="mt-1 text-sm text-muted-foreground">{t('jobs.intro')}</p>
	</div>

	<!-- Today's budget -->
	<section class="mb-4 rounded-xl border px-4 py-3">
		<h2 class="mb-3 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
			{t('jobs.section_budget')}
		</h2>
		{#if !connected}
			<p class="text-sm text-muted-foreground">{t('jobs.budget.not_connected')}</p>
		{:else}
			<div class="grid gap-4 lg:grid-cols-2">
				<div>
					{#if partition}
						<BudgetBar {partition} />
					{:else}
						<div class="h-24 animate-pulse rounded-lg bg-card/40"></div>
					{/if}
				</div>
				<QuotaWidget />
			</div>
			<div class="mt-4 border-t pt-3">
				<h3 class="mb-2 text-xs font-medium text-muted-foreground">{t('jobs.usage.title')}</h3>
				<UsageChart history={usage} daily={partition?.daily_units ?? 0} />
			</div>
		{/if}
	</section>

	<!-- Downloads -->
	<section class="mb-4 rounded-xl border px-4 py-3">
		<div class="mb-2 flex flex-wrap items-center gap-2">
			<h2 class="min-w-0 flex-1 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
				{t('jobs.section_downloads')}
			</h2>
			{#if failedDownloads.length}
				<Button variant="ghost" size="sm" class="gap-1.5" disabled={retrying} onclick={retryDownloads}>
					<HugeiconsIcon icon={RefreshIcon} class="h-4 w-4 {retrying ? 'animate-spin' : ''}" />
					{t('jobs.downloads.retry_failed')}
				</Button>
			{/if}
			<Button variant="ghost" size="sm" class="gap-1.5" onclick={openFolder}>
				<HugeiconsIcon icon={FolderOpenIcon} class="h-4 w-4" />
				{t('jobs.downloads.open_folder')}
			</Button>
		</div>
		{#if !downloads.length && !downloading}
			<p class="text-sm text-muted-foreground">{t('jobs.downloads.empty')}</p>
		{:else}
			<div class="flex flex-wrap gap-2 text-xs">
				{#each DOWNLOAD_ORDER as s (s)}
					{#if downloadCounts[s]}
						<Badge variant={s === 'error' || s === 'missing' ? 'outline' : 'muted'}>
							{t(`jobs.downloads.summary_${s}`, { count: downloadCounts[s] })}
						</Badge>
					{/if}
				{/each}
			</div>
			{#if downloading}
				<div class="mt-2" role="status">
					<div class="h-1.5 overflow-hidden rounded-full bg-muted">
						<div class="h-full rounded-full bg-primary transition-[width]" style="width: {downloading.percent}%"></div>
					</div>
					<div class="mt-1 text-xs text-muted-foreground tabular-nums">
						{t('jobs.downloads.active', { id: downloading.video_id, percent: Math.round(downloading.percent) })}
						{#if downloading.speed}· {downloading.speed}{/if}
						{#if downloading.eta}· {downloading.eta}{/if}
					</div>
				</div>
			{/if}
		{/if}
	</section>

	{#if loading}
		<div class="mb-4 h-32 animate-pulse rounded-xl bg-card/40"></div>
		<div class="h-40 animate-pulse rounded-xl bg-card/40"></div>
	{:else if error}
		<ErrorState message={t('jobs.loading_failed', { error })} onRetry={reloadAll} />
	{:else}
		<!-- Queue -->
		<section class="mb-4 rounded-xl border">
			<div class="flex flex-wrap items-baseline gap-2 px-4 pt-3 pb-2">
				<h2 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
					{t('jobs.section_queue')}
				</h2>
				{#if summary}
					<span class="text-xs text-muted-foreground">{summary}</span>
				{/if}
			</div>
			{#if !active.length}
				<p class="px-4 pb-3 text-sm text-muted-foreground">{t('jobs.queue_empty')}</p>
			{:else}
				<div class="divide-y border-t">
					{#each active as job, i (job.id)}
						<JobRow
							{job}
							canUp={canMove(active, i, -1)}
							canDown={canMove(active, i, 1)}
							onmove={(delta) => move(i, delta)}
							onchanged={reloadAll}
						/>
					{/each}
				</div>
			{/if}
		</section>

		<!-- History -->
		<section class="rounded-xl border">
			<h2 class="px-4 pt-3 pb-2 text-xs font-semibold uppercase tracking-wide text-muted-foreground">
				{t('jobs.section_history')}
			</h2>
			{#if !history.length}
				<p class="px-4 pb-3 text-sm text-muted-foreground">{t('jobs.history_empty')}</p>
			{:else}
				{#each history as group (group.day)}
					<div class="border-t bg-muted/30 px-4 py-1.5 text-xs font-medium text-muted-foreground">
						{dayLabel(group.day)}
					</div>
					<div class="divide-y">
						{#each group.items as entry (key(entry))}
							{#if entry.type === 'job'}
								<JobRow job={entry.job} op={entry.op} onchanged={reloadAll} />
							{:else}
								{@const op = entry.op}
								<div class="flex flex-wrap items-center gap-3 px-4 py-2.5">
									<div class="min-w-0 flex-1">
										<div class="text-sm font-medium">
											{t(`jobs.op.${op.kind}`, { count: op.summary.count })}
										</div>
										<div class="truncate text-xs text-muted-foreground">
											{timeOf(op.createdAt)}
											{#if op.summary.playlists.length}· {t('jobs.op.in', { playlists: opPlaylists(op) })}{/if}
										</div>
									</div>
									{#if op.undone}
										<Badge variant="muted">{t('jobs.op.undone')}</Badge>
									{:else if op.undoable}
										<Button variant="ghost" size="sm" class="gap-1" disabled={undoing !== null} onclick={() => undo(op)}>
											<HugeiconsIcon icon={UndoIcon} class="h-4 w-4" />
											{t('jobs.row.undo')}
										</Button>
									{:else}
										<span class="text-xs text-muted-foreground">{t('jobs.op.local')}</span>
									{/if}
								</div>
							{/if}
						{/each}
					</div>
				{/each}
			{/if}
		</section>
	{/if}
</div>
