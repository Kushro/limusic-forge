<script lang="ts">
	// Recover tracks, step 5 (F4.6): put the approved replacements in. "Replace" (the default) adds
	// each where its dead track is and then takes the dead one out (D1, D2: never before its
	// replacement is in); a track already gone from the playlist gets its replacement at the position
	// it last had, or at the end. "Only add" appends and leaves the dead tracks be. Every playlist
	// edited is one journal entry, and one Undo in the toast takes them all back.
	import { onMount } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { AlertCircleIcon, ArrowLeft01Icon, CheckmarkCircle02Icon } from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import { appliedTotals, approvedKeys, keyPlaylists, splitKey } from '$lib/recover';
	import { goToStep, loadCandidates, rec, recoverError, selectedKeys, trackRecover } from '$lib/recover.svelte';
	import { announceOps } from '$lib/playlistops.svelte';
	import { library, personal } from '$lib/player.svelte';
	import { mergeSaved } from '$lib/personal';
	import type { PlaylistEngine } from '$lib/ytdata.svelte';
	import { t } from '$lib/i18n.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import * as RadioGroup from '$lib/components/ui/radio-group';
	import EngineChoice from '$lib/components/ytdata/EngineChoice.svelte';

	let { onback }: { onback: () => void } = $props();

	type Action = 'replace' | 'append';
	const ACTIONS: Action[] = ['replace', 'append'];

	const names = $derived(
		new Map(mergeSaved(personal, library.items, 'playlist').map((p) => [p.id, p.title]))
	);
	const nameOf = (id: string) => names.get(id) ?? t('common.playlist_singular');
	const byKey = $derived(new Map((rec.candidates ?? []).map((c) => [c.key, c])));

	let rows = $state<api.RecoverRow[] | null>(null);
	let action = $state<Action>('replace');
	/** This write's engine, once the user picks one; unset follows `playlist_engine`. */
	let engine = $state<PlaylistEngine | undefined>();
	let busy = $state(false);
	let error = $state<string | null>(null);
	let result = $state<ReturnType<typeof appliedTotals> | null>(null);

	const keys = $derived(approvedKeys(rows ?? []));
	const playlists = $derived(keyPlaylists(keys));
	const removable = $derived(keys.filter((k) => byKey.get(k)?.in_playlist ?? true).length);
	const priced = $derived(
		action === 'replace'
			? [
					{ label: t('recover.cost_add'), kind: 'copy' as const, rows: keys.length, playlists },
					{ label: t('recover.cost_remove'), kind: 'remove' as const, rows: removable, playlists }
				]
			: [{ kind: 'copy' as const, rows: keys.length, playlists }]
	);
	const progress = $derived(busy && rec.snapshot?.phase === 'applying' ? rec.snapshot : null);

	onMount(() => {
		trackRecover();
		api
			.recoverRows(selectedKeys())
			.then((r) => (rows = r))
			.catch((e) => (error = recoverError(e)));
	});

	async function apply() {
		if (busy || !keys.length) return;
		const [what, choice, wanted] = [action, engine, [...keys]];
		busy = true;
		error = null;
		rec.snapshot = null;
		rec.ran = 'applying';
		try {
			const r = appliedTotals(await api.recoverApply(wanted, what, choice));
			result = r;
			const msg = [
				t(what === 'replace' ? 'recover.applied_replaced' : 'recover.applied_added', {
					count: r.done,
					playlists: r.playlists
				}),
				r.failed.length ? t('recover.applied_failed', { count: r.failed.length }) : ''
			]
				.filter(Boolean)
				.join(' · ');
			announceOps(r.ops, msg);
			// What was recovered is no longer a candidate; what failed stays selected for another go.
			void loadCandidates();
		} catch (e) {
			error = recoverError(e);
		} finally {
			busy = false;
			rec.ran = null;
		}
	}

	const label = (key: string) => byKey.get(key)?.title ?? splitKey(key).video_id;
	const pct = (done: number, all: number) => (all ? Math.round((done / all) * 100) : 100);
</script>

<div class="flex max-w-2xl flex-col gap-4">
	{#if result}
		<div class="flex items-start gap-3 rounded-xl border p-4">
			<HugeiconsIcon icon={CheckmarkCircle02Icon} class="mt-0.5 h-5 w-5 shrink-0 text-primary" />
			<div class="min-w-0">
				<p class="text-sm font-medium">{t('recover.result_done', { count: result.done, playlists: result.playlists })}</p>
				<p class="text-xs text-muted-foreground">{t('recover.result_undo_hint')}</p>
			</div>
		</div>
		{#if result.failed.length}
			<div class="flex flex-col gap-2">
				<p class="text-sm font-medium">{t('recover.result_failed', { count: result.failed.length })}</p>
				<div class="overflow-x-auto rounded-xl border">
					<table class="w-full text-sm">
						<thead class="bg-muted/40 text-left text-xs text-muted-foreground">
							<tr>
								<th class="px-3 py-2 font-medium">{t('recover.col_track')}</th>
								<th class="px-3 py-2 font-medium">{t('recover.col_playlist')}</th>
								<th class="px-3 py-2 font-medium">{t('recover.col_error')}</th>
							</tr>
						</thead>
						<tbody class="divide-y">
							{#each result.failed as f (f.key)}
								<tr>
									<td class="max-w-56 truncate px-3 py-2">{label(f.key)}</td>
									<td class="max-w-48 truncate px-3 py-2 text-muted-foreground">{nameOf(splitKey(f.key).playlist_id)}</td>
									<td class="px-3 py-2 text-destructive">{recoverError(f.error)}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</div>
		{/if}
		<div class="flex flex-wrap gap-2">
			<Button onclick={() => goToStep('pick')}>{t('recover.back_to_start')}</Button>
			{#if result.failed.length}
				<Button variant="outline" onclick={() => goToStep('review')}>{t('recover.review_failed')}</Button>
			{/if}
		</div>
	{:else}
		<p class="text-sm">
			{t('recover.apply_summary', { count: keys.length, playlists: playlists.length })}
		</p>

		<div>
			<p class="text-xs font-medium text-muted-foreground">{t('recover.action')}</p>
			<RadioGroup.Root
				value={action}
				onValueChange={(v) => (action = v === 'append' ? 'append' : 'replace')}
				class="mt-1 grid gap-2 sm:grid-cols-2"
				aria-label={t('recover.action')}
				disabled={busy}
			>
				{#each ACTIONS as a (a)}
					<label class="flex cursor-pointer items-start gap-3 rounded-2xl border px-3 py-2 transition-colors hover:bg-accent/10 has-[[data-state=checked]]:border-primary has-[[data-state=checked]]:bg-primary/5">
						<RadioGroup.Item value={a} class="mt-0.5" />
						<span class="min-w-0">
							<span class="flex items-center gap-2 text-sm font-medium">
								{t(`recover.action_${a}`)}
								{#if a === 'replace'}<Badge variant="label">{t('recover.recommended')}</Badge>{/if}
							</span>
							<span class="block text-xs text-muted-foreground">{t(`recover.action_${a}_hint`)}</span>
						</span>
					</label>
				{/each}
			</RadioGroup.Root>
			<EngineChoice bind:choice={engine} ops={priced} />
		</div>

		{#if progress}
			<div class="flex flex-col gap-2">
				<span class="text-sm tabular-nums text-muted-foreground">
					{t('recover.apply_progress', { done: progress.done, total: progress.total })}
				</span>
				<div class="h-2 w-full overflow-hidden rounded-full bg-muted">
					<div class="h-full rounded-full bg-primary transition-[width] duration-300" style="width: {pct(progress.done, progress.total)}%"></div>
				</div>
			</div>
		{/if}

		{#if error}
			<p class="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/5 px-3 py-2 text-sm">
				<HugeiconsIcon icon={AlertCircleIcon} class="mt-0.5 h-4 w-4 shrink-0" />
				{error}
			</p>
		{/if}

		<div class="flex flex-wrap items-center gap-2">
			<Button variant="outline" size="sm" onclick={onback} disabled={busy}>
				<HugeiconsIcon icon={ArrowLeft01Icon} class="h-4 w-4" />
				{t('recover.back')}
			</Button>
			<Button class="ml-auto" onclick={apply} disabled={busy || !rows || !keys.length}>
				{busy ? t('recover.applying') : t('recover.apply', { count: keys.length })}
			</Button>
		</div>
	{/if}
</div>
