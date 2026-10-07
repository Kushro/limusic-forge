<script lang="ts">
	// Duplicates in one playlist (`playlist_tools/dedup.rs`): choose how hard to look, review what
	// was found with one copy kept per group, untick anything you'd rather keep, remove the rest.
	// PlaylistForge's Filter → Preview → Confirm, in two steps, since nothing here costs quota.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { ArrowLeft01Icon, Copy01Icon } from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { DuplicateKeep, DuplicateReport, SongItem } from '$lib/api';
	import { anchorsFor } from '$lib/reorder';
	import { announceOp } from '$lib/playlistops.svelte';
	import { bumpLibraryTrackCount, noteUnsavedFrom, toast } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';
	import { thumb } from '$lib/thumb';
	import { Badge } from '../ui/badge';
	import { Button } from '../ui/button';
	import { Checkbox } from '../ui/checkbox';
	import * as RadioGroup from '../ui/radio-group';

	let {
		playlistId,
		title,
		editable,
		ondone
	}: { playlistId: string; title: string; editable: boolean; ondone: () => void } = $props();

	let exact = $state(true);
	let byTitle = $state(true);
	let similar = $state(true);
	let keep = $state<DuplicateKeep>('first');
	let searching = $state(false);
	let removing = $state(false);
	let report = $state.raw<DuplicateReport | null>(null);
	// Rows (indices into report.rows) marked for removal; starts as every copy but the kept one.
	let marked = $state<Set<number>>(new Set());
	const KEEPS: DuplicateKeep[] = ['first', 'last', 'prefer_song'];

	async function search() {
		if (searching) return;
		searching = true;
		try {
			const r = await api.findDuplicates(playlistId, { exact, title: byTitle, similar }, keep);
			report = r;
			marked = new Set(r.clusters.flatMap((c) => c.rows.filter((i) => i !== c.keep)));
		} catch (e) {
			toast.error(String(e));
		} finally {
			searching = false;
		}
	}

	/** Keep this row of its group: the others are marked, it is not. */
	function keepRow(cluster: number[], row: number) {
		const next = new Set(marked);
		for (const i of cluster) {
			if (i === row) next.delete(i);
			else next.add(i);
		}
		marked = next;
	}

	function toggle(row: number, on: boolean) {
		const next = new Set(marked);
		if (on) next.add(row);
		else next.delete(row);
		marked = next;
	}

	async function remove() {
		if (!report || !marked.size || removing) return;
		removing = true;
		const rows = report.rows;
		const targets: SongItem[] = [...marked].sort((a, b) => a - b).map((i) => rows[i]);
		const handle = (s: SongItem) => s.set_video_id ?? '';
		const anchors = anchorsFor(rows.map(handle), new Set(targets.map(handle)));
		try {
			const op = await api.removeTracks(
				playlistId,
				title,
				targets.map((song) => ({ song, before: anchors.get(handle(song)) ?? null })),
				'dedupe'
			);
			bumpLibraryTrackCount(playlistId, -targets.length);
			// A video still in the playlist (the kept copy) stays marked as saved there.
			const left = new Set(rows.filter((r) => !targets.includes(r)).map((r) => r.video_id));
			for (const s of targets) if (!left.has(s.video_id)) noteUnsavedFrom(playlistId, s.video_id);
			announceOp(
				op,
				targets.length === 1
					? t('dedup.removed_one')
					: t('dedup.removed', { count: targets.length })
			);
			ondone();
		} catch (e) {
			toast.error(String(e));
		} finally {
			removing = false;
		}
	}

	const total = $derived(report?.clusters.reduce((n, c) => n + c.rows.length - 1, 0) ?? 0);
</script>

{#if !report}
	<div class="space-y-4">
		<p class="text-sm text-muted-foreground">{t('dedup.intro')}</p>
		<div class="space-y-2">
			{#each [
				{ get: () => exact, set: (v: boolean) => (exact = v), key: 'exact' },
				{ get: () => byTitle, set: (v: boolean) => (byTitle = v), key: 'title' },
				{ get: () => similar, set: (v: boolean) => (similar = v), key: 'similar' }
			] as level (level.key)}
				<label class="flex cursor-pointer items-start gap-3 rounded-lg border p-3 hover:bg-accent/5">
					<Checkbox checked={level.get()} onCheckedChange={(v) => level.set(!!v)} class="mt-0.5" />
					<span>
						<span class="block text-sm font-medium">{t(`dedup.level_${level.key as 'exact' | 'title' | 'similar'}`)}</span>
						<span class="block text-xs text-muted-foreground">
							{t(`dedup.level_${level.key as 'exact' | 'title' | 'similar'}_hint`)}
						</span>
					</span>
				</label>
			{/each}
		</div>
		<div>
			<p class="mb-1 text-xs font-medium text-muted-foreground">{t('dedup.keep')}</p>
			<RadioGroup.Root value={keep} onValueChange={(v) => (keep = v as DuplicateKeep)} class="gap-1">
				{#each KEEPS as k (k)}
					<label class="flex cursor-pointer items-center gap-2 text-sm">
						<RadioGroup.Item value={k} />
						{t(`dedup.keep_${k}`)}
					</label>
				{/each}
			</RadioGroup.Root>
		</div>
		<div class="flex justify-end">
			<Button onclick={search} disabled={searching || !(exact || byTitle || similar)} class="gap-2">
				<HugeiconsIcon icon={Copy01Icon} class="h-4 w-4" />
				{searching ? t('dedup.searching') : t('dedup.search')}
			</Button>
		</div>
	</div>
{:else}
	<div class="flex min-h-0 flex-col gap-3">
		<div class="flex items-center gap-2">
			<Button variant="ghost" size="icon-sm" onclick={() => (report = null)} aria-label={t('common.back')}>
				<HugeiconsIcon icon={ArrowLeft01Icon} class="h-4 w-4" />
			</Button>
			<p class="text-sm" role="status">
				{!report.clusters.length
					? t('dedup.none')
					: total === 1
						? t('dedup.found_one')
						: t('dedup.found', { count: total, groups: report.clusters.length })}
			</p>
		</div>
		{#if report.clusters.length}
			<div class="max-h-[50vh] space-y-3 overflow-y-auto pr-1">
				{#each report.clusters as c, ci (c.rows[0])}
					<section class="rounded-lg border p-2">
						<div class="mb-1 flex flex-wrap gap-1 px-1">
							{#each c.reasons as r (r)}
								<Badge variant="chip">{t(`dedup.reason_${r}`)}</Badge>
							{/each}
						</div>
						<RadioGroup.Root
							value={String(c.rows.find((i) => !marked.has(i)) ?? -1)}
							onValueChange={(v) => keepRow(c.rows, Number(v))}
							class="gap-0"
						>
							{#each c.rows as i (i)}
								{@const s = report.rows[i]}
								<div class="flex items-center gap-3 rounded-md px-1 py-1 {marked.has(i) ? 'opacity-60' : ''}">
									<RadioGroup.Item value={String(i)} aria-label={t('dedup.keep_this')} />
									{#if s.thumbnail}
										<img src={thumb(s.thumbnail, 96)} alt="" class="h-8 w-8 shrink-0 rounded object-cover" />
									{:else}
										<span class="h-8 w-8 shrink-0 rounded bg-muted"></span>
									{/if}
									<span class="min-w-0 flex-1">
										<span class="block truncate text-sm">{s.title}</span>
										<span class="block truncate text-xs text-muted-foreground">
											{s.artists}{s.duration ? ` · ${s.duration}` : ''} · #{i + 1}{s.is_video ? ` · ${t('dedup.video')}` : ''}
										</span>
									</span>
									<label class="flex items-center gap-1.5 text-xs text-muted-foreground">
										<Checkbox checked={marked.has(i)} onCheckedChange={(v) => toggle(i, !!v)} />
										{t('dedup.remove_this')}
									</label>
								</div>
							{/each}
						</RadioGroup.Root>
					</section>
				{/each}
			</div>
			<div class="flex items-center justify-end gap-2">
				{#if !editable}
					<p class="mr-auto text-xs text-muted-foreground">{t('dedup.not_yours')}</p>
				{/if}
				<Button
					variant="destructive"
					onclick={remove}
					disabled={!editable || !marked.size || removing}
				>
					{removing
						? t('common.loading')
						: marked.size === 1
							? t('dedup.remove_one')
							: t('dedup.remove', { count: marked.size })}
				</Button>
			</div>
		{/if}
	</div>
{/if}
