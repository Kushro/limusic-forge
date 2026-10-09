<script lang="ts">
	// Recover tracks, steps 2 and 3 (F4.3-F4.4): recover the titles the selection lacks (locally, and
	// with the Wayback Machine when allowed), then search a replacement for each. Both run in Rust's
	// background and report through `recover-progress` (`rec.snapshot`, followed for as long as the
	// app runs), so leaving the page halfway finds the run where it is. A run that ends `done` moves
	// on by itself; a cancelled or failed one says why and can start again (it resumes: Rust skips
	// what is already titled or searched).
	import { onMount, untrack } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { AlertCircleIcon, ArrowLeft01Icon, ArrowRight01Icon } from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import { countdown, searchableKeys, splitKey, untitledKeys } from '$lib/recover';
	import { rec, recoverError, selectedKeys, startRun, trackRecover } from '$lib/recover.svelte';
	import { cooldownUntil } from '$lib/import.svelte';
	import { t } from '$lib/i18n.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Switch } from '$lib/components/ui/switch';

	let { onnext, onback }: { onnext: () => void; onback: () => void } = $props();

	// The page remounts this per step ({#key}), so the step is fixed for this instance.
	const titles = untrack(() => rec.step === 'titles');
	const phase: api.RecoverPhase = titles ? 'titles' : 'searching';

	let rows = $state<api.RecoverRow[] | null>(null);
	let wayback = $state(true);
	let error = $state<string | null>(null);
	let blockedUntil = $state<number | null>(null);
	let starting = $state(false);

	/** This step's run, when the last one started was this step's. */
	const snap = $derived(rec.ran === phase ? rec.snapshot : null);
	const running = $derived(!!snap && snap.phase === phase);
	const ended = $derived(snap && (snap.phase === 'cancelled' || snap.phase === 'failed') ? snap : null);
	const untitled = $derived(rows ? untitledKeys(rows) : []);
	const searchable = $derived(rows ? searchableKeys(rows) : []);
	const total = $derived(rows?.length ?? rec.selected.size);

	const byKey = $derived(new Map((rec.candidates ?? []).map((c) => [c.key, c])));
	const label = (k: string) => {
		const c = byKey.get(k);
		return c?.title ?? (k.includes('\u001f') ? splitKey(k).video_id : k);
	};

	async function loadRows() {
		try {
			rows = await api.recoverRows(selectedKeys());
		} catch (e) {
			error = recoverError(e);
		}
	}

	onMount(() => {
		trackRecover();
		void loadRows();
	});

	// A run that went through moves on; one that stopped shows what it got to.
	$effect(() => {
		const s = rec.snapshot;
		if (rec.ran !== phase || !s) return;
		if (s.phase === 'done') {
			untrack(() => {
				rec.ran = null;
				onnext();
			});
		} else if (s.phase === 'cancelled' || s.phase === 'failed') {
			untrack(() => {
				blockedUntil = cooldownUntil(s.message);
				void loadRows();
			});
		}
	});

	// The clock for every countdown on screen: a pause, or a cooldown before "Try again".
	let now = $state(Date.now());
	const ticking = $derived(snap?.waiting_until ?? blockedUntil);
	$effect(() => {
		if (!ticking) return;
		now = Date.now();
		const timer = setInterval(() => (now = Date.now()), 1000);
		return () => clearInterval(timer);
	});
	const blocked = $derived(!!blockedUntil && blockedUntil * 1000 > now);

	async function start() {
		if (starting || running) return;
		starting = true;
		error = null;
		blockedUntil = null;
		try {
			const keys = selectedKeys();
			await startRun(phase, () => (titles ? api.recoverTitles(keys, wayback) : api.recoverSearch(keys)));
		} catch (e) {
			blockedUntil = cooldownUntil(String(e ?? ''));
			error = recoverError(e);
		} finally {
			starting = false;
		}
	}

	const pct = (done: number, all: number) => (all ? Math.round((done / all) * 100) : 100);
</script>

<div class="flex max-w-2xl flex-col gap-4">
	{#if snap && running}
		<div class="flex flex-col gap-3 rounded-xl border p-4">
			<div class="flex items-baseline justify-between gap-3">
				<span class="text-lg font-semibold tabular-nums">
					{t(titles ? 'recover.titles_progress' : 'recover.search_progress', { done: snap.done, total: snap.total })}
				</span>
				<span class="text-xs tabular-nums text-muted-foreground">{pct(snap.done, snap.total)}%</span>
			</div>
			<div
				class="h-2 w-full overflow-hidden rounded-full bg-muted"
				role="progressbar"
				aria-valuemin={0}
				aria-valuemax={snap.total}
				aria-valuenow={snap.done}
			>
				<div class="h-full rounded-full bg-primary transition-[width] duration-300" style="width: {pct(snap.done, snap.total)}%"></div>
			</div>
			{#if snap.current}
				<p class="truncate text-sm text-muted-foreground">{t('recover.current', { name: label(snap.current) })}</p>
			{/if}
			{#if snap.waiting_until}
				<p class="flex items-start gap-2 text-xs text-amber-500">
					<HugeiconsIcon icon={AlertCircleIcon} class="mt-px h-3.5 w-3.5 shrink-0" />
					{snap.message === 'wayback_rate_limited'
						? t('recover.wayback_paused', { time: countdown(snap.waiting_until, now) })
						: t('recover.search_paused', { time: countdown(snap.waiting_until, now) })}
				</p>
			{/if}
			<p class="text-xs text-muted-foreground">{t(titles ? 'recover.titles_pace' : 'recover.search_pace')}</p>
			<div>
				<Button variant="outline" size="sm" onclick={() => api.recoverCancel().catch(() => {})}>{t('recover.cancel')}</Button>
			</div>
		</div>
	{:else}
		{#if ended}
			<div class="flex items-start gap-2 rounded-xl border p-3 text-sm {ended.phase === 'failed' ? 'border-destructive/30 bg-destructive/5' : ''}">
				<HugeiconsIcon icon={AlertCircleIcon} class="mt-0.5 h-4 w-4 shrink-0" />
				<div class="min-w-0">
					<p>
						{ended.phase === 'cancelled'
							? t('recover.run_cancelled', { done: ended.done, total: ended.total })
							: t('recover.run_failed', { error: recoverError(ended.message) })}
					</p>
					{#if blocked && blockedUntil}
						<p class="mt-1 text-xs text-muted-foreground">{t('recover.try_again_in', { time: countdown(blockedUntil, now) })}</p>
					{/if}
				</div>
			</div>
		{/if}

		{#if !rows}
			<p class="animate-pulse text-sm text-muted-foreground">{t('recover.loading_rows')}</p>
		{:else if titles}
			{#if !untitled.length}
				<p class="text-sm">{t('recover.titles_none_missing', { total })}</p>
			{:else}
				<p class="text-sm">{t('recover.titles_missing', { count: untitled.length, total })}</p>
				<p class="text-xs text-muted-foreground">{t('recover.titles_local_hint')}</p>
				<label class="flex items-start gap-3 rounded-xl border p-3">
					<Switch checked={wayback} onCheckedChange={(v) => (wayback = v)} class="mt-0.5" />
					<span class="min-w-0">
						<span class="block text-sm font-medium">{t('recover.wayback')}</span>
						<span class="block text-xs text-muted-foreground">{t('recover.wayback_hint')}</span>
					</span>
				</label>
			{/if}
		{:else}
			<p class="text-sm">{t('recover.search_count', { count: searchable.length, total })}</p>
			{#if untitled.length}
				<p class="text-xs text-muted-foreground">{t('recover.search_untitled', { count: untitled.length })}</p>
			{/if}
			{#if !searchable.length}
				<p class="text-xs text-muted-foreground">{t('recover.search_nothing')}</p>
			{/if}
		{/if}

		{#if error}
			<p class="rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm">{error}</p>
		{/if}

		<div class="flex flex-wrap items-center gap-2">
			<Button variant="outline" size="sm" onclick={onback}>
				<HugeiconsIcon icon={ArrowLeft01Icon} class="h-4 w-4" />
				{t('recover.back')}
			</Button>
			<div class="ml-auto flex flex-wrap items-center gap-2">
				{#if titles ? untitled.length : searchable.length}
					<Button variant="ghost" size="sm" onclick={onnext}>
						{t(titles ? 'recover.skip_titles' : 'recover.skip_search')}
					</Button>
					<Button onclick={start} disabled={starting || !rows || blocked}>
						{ended ? t('recover.resume') : t(titles ? 'recover.titles_start' : 'recover.search_start')}
						<HugeiconsIcon icon={ArrowRight01Icon} class="h-4 w-4" />
					</Button>
				{:else}
					<Button onclick={onnext}>
						{t('recover.next')}
						<HugeiconsIcon icon={ArrowRight01Icon} class="h-4 w-4" />
					</Button>
				{/if}
			</div>
		</div>
	{/if}
</div>
