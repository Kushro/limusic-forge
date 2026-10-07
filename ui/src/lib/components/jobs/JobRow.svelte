<script lang="ts">
	// One job on the jobs page: its title, state, engine and progress, and what can be done with it.
	// In the queue: pause or resume, cancel (optionally undoing what it already did), retry its
	// failed items, change its priority and move it within its priority. In the history: undo it,
	// through its journal entry when it has one (the same Undo the toast offers), else by queueing
	// its revert. "Details" lists its items with their state and error.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		ArrowDown01Icon,
		ArrowUp01Icon,
		Cancel01Icon,
		PauseIcon,
		PlayIcon,
		RefreshIcon,
		UndoIcon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { JobDetail, JobView, PlaylistOp } from '$lib/api';
	import { t } from '$lib/i18n.svelte';
	import { toast } from '$lib/player.svelte';
	import { undoOp } from '$lib/playlistops.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Checkbox } from '$lib/components/ui/checkbox';
	import * as Select from '$lib/components/ui/select';
	import {
		PRIORITIES,
		canCancel,
		canPause,
		canPrioritize,
		canResume,
		canRetry,
		canRevert,
		isEnded,
		jobTitle,
		priorityKey,
		progress,
		statusHintKey,
		statusKey,
		statusTone,
		type Tone
	} from './jobs';

	let {
		job,
		op = null,
		canUp = false,
		canDown = false,
		onmove,
		onchanged
	}: {
		job: JobView;
		/** Its undo-journal entry (history rows). */
		op?: PlaylistOp | null;
		canUp?: boolean;
		canDown?: boolean;
		onmove?: (delta: -1 | 1) => void;
		/** Something changed it: the page reloads (the backend also sends `jobs-changed`). */
		onchanged?: () => void;
	} = $props();

	const TONE: Record<Tone, string> = {
		neutral: 'bg-muted text-muted-foreground',
		info: 'bg-sky-500/15 text-sky-700 dark:text-sky-300',
		warning: 'bg-amber-500/15 text-amber-700 dark:text-amber-300',
		error: 'bg-destructive/15 text-destructive',
		success: 'bg-emerald-500/15 text-emerald-700 dark:text-emerald-300'
	};

	const title = $derived(jobTitle(job));
	const p = $derived(progress(job));
	const ended = $derived(isEnded(job.status));
	const whenOf = (iso: string | null) =>
		iso ? new Date(iso).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' }) : '';

	let busy = $state(false);
	async function act(run: () => Promise<unknown>, done?: string) {
		if (busy) return;
		busy = true;
		try {
			await run();
			if (done) toast.success(done);
			onchanged?.();
		} catch (e) {
			toast.error(t('jobs.toast.failed', { error: String(e) }));
		} finally {
			busy = false;
		}
	}

	// --- details ------------------------------------------------------------------------------------
	let open = $state(false);
	let detail = $state<JobDetail | null>(null);
	let failedOnly = $state(false);
	async function loadDetail() {
		try {
			detail = await api.jobDetail(job.id);
		} catch (e) {
			toast.error(t('jobs.toast.failed', { error: String(e) }));
		}
	}
	function toggleDetail() {
		open = !open;
	}
	// Opening reads the items; a job that moves on while they are open is read again.
	$effect(() => {
		void job.done_items;
		void job.failed_items;
		void job.status;
		if (open) void loadDetail();
	});
	const items = $derived(
		(detail?.items ?? []).filter((i) => !failedOnly || i.status === 'failed' || i.status === 'skipped')
	);

	// --- cancel / undo ------------------------------------------------------------------------------
	// One confirmation for both: cancelling an active job (revert optional), or undoing an ended one.
	let confirming = $state<'cancel' | 'undo' | null>(null);
	let revert = $state(false);
	async function ask(kind: 'cancel' | 'undo') {
		revert = false;
		confirming = kind;
		if (!detail) await loadDetail();
	}
	const preview = $derived(detail?.revert ?? null);
	function confirm() {
		const kind = confirming;
		confirming = null;
		if (kind === 'cancel')
			void act(
				() => api.jobCancel(job.id, revert),
				revert ? t('jobs.toast.cancelled_revert') : t('jobs.toast.cancelled')
			);
		else if (kind === 'undo') void act(() => api.jobCancel(job.id, true), t('jobs.toast.undo_queued'));
	}
	function undo() {
		const id = op?.id;
		if (id !== undefined) void act(() => undoOp(id));
		else void ask('undo');
	}
	const undoAvailable = $derived(op ? op.undoable : canRevert(job));

	async function retry() {
		if (busy) return;
		busy = true;
		try {
			const n = await api.jobRetryFailed(job.id);
			toast.success(n ? t('jobs.toast.retried', { count: n }) : t('jobs.toast.retried_none'));
			onchanged?.();
		} catch (e) {
			toast.error(t('jobs.toast.failed', { error: String(e) }));
		} finally {
			busy = false;
		}
	}
</script>

<div class="px-4 py-3">
	<div class="flex flex-wrap items-start gap-3">
		<div class="min-w-0 flex-1">
			<div class="flex flex-wrap items-center gap-2">
				<span class="min-w-0 truncate text-sm font-medium" title={t(title.key, title.params)}>
					{t(title.key, title.params)}
				</span>
				<span class="inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-[11px] font-medium {TONE[statusTone(job.status)]}">
					{t(statusKey(job.status))}
				</span>
				<Badge variant="outline">{t(`jobs.engine.${job.engine}`)}</Badge>
				{#if job.priority === 0}
					<Badge variant="muted">{t(priorityKey(job.priority))}</Badge>
				{/if}
			</div>
			<p class="mt-0.5 text-xs text-muted-foreground">
				{#if job.status === 'waiting_quota' || job.status === 'paused_network'}
					{t(statusHintKey(job.status))}
					{#if job.resume_at}· {t('jobs.row.resumes', { when: whenOf(job.resume_at) })}{/if}
				{:else if job.status === 'failed' && job.last_error}
					{t(statusHintKey(job.status))} {job.last_error}
				{:else}
					{t(statusHintKey(job.status))}
				{/if}
			</p>
		</div>

		<div class="flex shrink-0 flex-wrap items-center gap-1">
			{#if !ended}
				{#if onmove}
					<Button variant="ghost" size="icon-sm" disabled={!canUp || busy} onclick={() => onmove?.(-1)} aria-label={t('jobs.row.move_up')} title={t('jobs.row.move_up')}>
						<HugeiconsIcon icon={ArrowUp01Icon} class="h-4 w-4" />
					</Button>
					<Button variant="ghost" size="icon-sm" disabled={!canDown || busy} onclick={() => onmove?.(1)} aria-label={t('jobs.row.move_down')} title={t('jobs.row.move_down')}>
						<HugeiconsIcon icon={ArrowDown01Icon} class="h-4 w-4" />
					</Button>
				{/if}
				{#if canPrioritize(job)}
					<Select.Root
						type="single"
						value={String(job.priority)}
						onValueChange={(v) =>
							v && Number(v) !== job.priority && act(() => api.jobSetPriority(job.id, Number(v)))}
					>
						<Select.Trigger size="sm" class="w-28" aria-label={t('jobs.priority.label')}>
							<span class="flex-1 truncate text-left">{t(priorityKey(job.priority))}</span>
						</Select.Trigger>
						<Select.Content>
							{#each PRIORITIES as pr (pr)}
								<Select.Item value={String(pr)} label={t(priorityKey(pr))}>{t(priorityKey(pr))}</Select.Item>
							{/each}
						</Select.Content>
					</Select.Root>
				{/if}
				{#if canPause(job)}
					<Button variant="ghost" size="sm" class="gap-1" disabled={busy} onclick={() => act(() => api.jobPause(job.id), t('jobs.toast.paused'))}>
						<HugeiconsIcon icon={PauseIcon} class="h-4 w-4" />
						{t('jobs.row.pause')}
					</Button>
				{:else if canResume(job)}
					<Button variant="ghost" size="sm" class="gap-1" disabled={busy} onclick={() => act(() => api.jobResume(job.id), t('jobs.toast.resumed'))}>
						<HugeiconsIcon icon={PlayIcon} class="h-4 w-4" />
						{t('jobs.row.resume')}
					</Button>
				{/if}
			{/if}
			{#if canRetry(job)}
				<Button variant="ghost" size="sm" class="gap-1" disabled={busy} onclick={retry}>
					<HugeiconsIcon icon={RefreshIcon} class="h-4 w-4" />
					{t('jobs.row.retry')}
				</Button>
			{/if}
			{#if !ended && canCancel(job)}
				<Button variant="ghost" size="sm" class="gap-1 text-destructive" disabled={busy} onclick={() => ask('cancel')}>
					<HugeiconsIcon icon={Cancel01Icon} class="h-4 w-4" />
					{t('jobs.row.cancel')}
				</Button>
			{/if}
			{#if ended}
				{#if job.revert_requested || job.revert_job_id !== null}
					<Badge variant="muted">{t('jobs.row.undo_pending')}</Badge>
				{:else if op?.undone}
					<Badge variant="muted">{t('jobs.row.undone')}</Badge>
				{:else if undoAvailable}
					<Button variant="ghost" size="sm" class="gap-1" disabled={busy} onclick={undo}>
						<HugeiconsIcon icon={UndoIcon} class="h-4 w-4" />
						{t('jobs.row.undo')}
					</Button>
				{/if}
			{/if}
			<Button variant="ghost" size="sm" onclick={toggleDetail}>
				{open ? t('jobs.row.hide_details') : t('jobs.row.details')}
			</Button>
		</div>
	</div>

	<!-- Progress -->
	{#if !ended || job.failed_items || job.skipped_items}
		<div class="mt-2">
			<div
				class="h-1.5 overflow-hidden rounded-full bg-muted"
				role="progressbar"
				aria-valuemin={0}
				aria-valuemax={p.total}
				aria-valuenow={p.settled}
			>
				<div class="h-full rounded-full bg-primary transition-[width]" style="width: {p.fraction * 100}%"></div>
			</div>
		</div>
	{/if}
	<div class="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-xs text-muted-foreground tabular-nums">
		<span>{t('jobs.row.progress', { settled: p.settled, total: p.total })}</span>
		{#if job.engine === 'ytdata'}
			<span>{t('jobs.row.units', { spent: job.spent_units.toLocaleString(), estimate: job.est_units_total.toLocaleString() })}</span>
		{/if}
		{#if job.failed_items || job.skipped_items}
			<span class="text-destructive">{t('jobs.row.problems', { failed: job.failed_items, skipped: job.skipped_items })}</span>
		{/if}
		{#if job.retried_items}
			<span>{t('jobs.row.retried', { count: job.retried_items })}</span>
		{/if}
		{#if job.total_phases > 1 && !ended}
			<span>{t('jobs.detail.phase', { phase: job.phase, total: job.total_phases })}</span>
		{/if}
		<span>
			{ended && job.finished_at
				? t('jobs.row.finished', { when: whenOf(job.finished_at) })
				: t('jobs.row.created', { when: whenOf(job.created_at) })}
		</span>
	</div>

	<!-- Cancel / undo confirmation -->
	{#if confirming}
		<div class="mt-3 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm" role="alertdialog" aria-label={confirming === 'cancel' ? t('jobs.cancel_dialog.title') : t('jobs.undo_dialog.title')}>
			{#if confirming === 'cancel'}
				<p class="font-medium">{t('jobs.cancel_dialog.title')}</p>
				<p class="mt-0.5 text-xs text-muted-foreground">{t('jobs.cancel_dialog.desc')}</p>
				{#if preview && preview.item_count > 0}
					<label class="mt-2 flex cursor-pointer items-center gap-2 text-xs">
						<Checkbox bind:checked={revert} />
						<span>
							{t('jobs.cancel_dialog.revert')}
							<span class="text-muted-foreground">
								({t('jobs.cancel_dialog.revert_cost', { count: preview.item_count, units: preview.estimated_units.toLocaleString() })})
							</span>
						</span>
					</label>
				{:else if preview}
					<p class="mt-2 text-xs text-muted-foreground">{t('jobs.cancel_dialog.revert_none')}</p>
				{/if}
				<div class="mt-2.5 flex justify-end gap-2">
					<Button variant="ghost" size="sm" onclick={() => (confirming = null)}>{t('jobs.cancel_dialog.keep')}</Button>
					<Button variant="destructive" size="sm" onclick={confirm}>{t('jobs.cancel_dialog.confirm')}</Button>
				</div>
			{:else}
				<p class="font-medium">{t('jobs.undo_dialog.title')}</p>
				<p class="mt-0.5 text-xs text-muted-foreground">
					{t('jobs.undo_dialog.desc', { count: preview?.item_count ?? 0, units: (preview?.estimated_units ?? 0).toLocaleString() })}
				</p>
				<div class="mt-2.5 flex justify-end gap-2">
					<Button variant="ghost" size="sm" onclick={() => (confirming = null)}>{t('jobs.cancel_dialog.keep')}</Button>
					<Button size="sm" disabled={!preview?.item_count} onclick={confirm}>{t('jobs.undo_dialog.confirm')}</Button>
				</div>
			{/if}
		</div>
	{/if}

	<!-- Items -->
	{#if open}
		<div class="mt-3 rounded-lg border">
			{#if !detail}
				<p class="px-3 py-2 text-xs text-muted-foreground">{t('jobs.detail.loading')}</p>
			{:else}
				<div class="flex items-center justify-end border-b px-3 py-1.5">
					<label class="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
						<Checkbox bind:checked={failedOnly} />
						{t('jobs.detail.failed_only')}
					</label>
				</div>
				{#if !items.length}
					<p class="px-3 py-2 text-xs text-muted-foreground">{t('jobs.detail.empty')}</p>
				{:else}
					<div class="max-h-72 overflow-auto">
						<table class="w-full text-xs">
							<thead class="sticky top-0 bg-background text-left text-muted-foreground">
								<tr class="border-b">
									<th class="px-3 py-1.5 font-medium">{t('jobs.detail.col_track')}</th>
									<th class="px-2 py-1.5 font-medium">{t('jobs.detail.col_action')}</th>
									<th class="px-2 py-1.5 font-medium">{t('jobs.detail.col_status')}</th>
									<th class="px-2 py-1.5 text-right font-medium">{t('jobs.detail.col_attempts')}</th>
									<th class="px-3 py-1.5 font-medium">{t('jobs.detail.col_error')}</th>
								</tr>
							</thead>
							<tbody>
								{#each items as it (it.id)}
									<tr class="border-b last:border-b-0">
										<td class="px-3 py-1 font-mono">{it.video_id ?? '—'}</td>
										<td class="px-2 py-1 font-mono text-muted-foreground">{it.action}</td>
										<td class="px-2 py-1 whitespace-nowrap {it.status === 'failed' ? 'text-destructive' : ''}">{t(`jobs.item_status.${it.status}`)}</td>
										<td class="px-2 py-1 text-right tabular-nums">{it.attempts}</td>
										<td class="px-3 py-1 text-muted-foreground">{it.last_error ?? ''}</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}
			{/if}
		</div>
	{/if}
</div>
