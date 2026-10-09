<script lang="ts">
	// Recover tracks, step 4 (F4.5): every selected row with the replacement the search proposed, to
	// approve, reject or change (the import's review, ImportDialog, laid out as a page). A row click
	// opens it: the other candidates, a search of your own, a pasted link or ID, and a title typed in
	// (for the ones the Wayback Machine could not name) that searches that row again. Only approved
	// rows with a replacement go on to the apply step; the rows themselves are Rust's session.
	import { onMount, untrack } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		ArrowDown01Icon,
		ArrowLeft01Icon,
		ArrowRight01Icon,
		ArrowRight02Icon,
		ArrowUp01Icon,
		Cancel01Icon,
		Link01Icon,
		MusicNote01Icon,
		PlayIcon,
		Search01Icon,
		Tick02Icon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import {
		approvableRows,
		approvedKeys,
		filterRows,
		pastedVideoId,
		positionLabel,
		REVIEW_TIERS,
		splitKey,
		tierCounts,
		type ReviewFilter,
		type ReviewTier
	} from '$lib/recover';
	import { rec, recoverError, selectedKeys, startRun, trackRecover } from '$lib/recover.svelte';
	import { library, personal, toast } from '$lib/player.svelte';
	import { mergeSaved } from '$lib/personal';
	import { thumb } from '$lib/thumb';
	import { t } from '$lib/i18n.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';

	let { onnext, onback }: { onnext: () => void; onback: () => void } = $props();

	const names = $derived(
		new Map(mergeSaved(personal, library.items, 'playlist').map((p) => [p.id, p.title]))
	);
	const nameOf = (id: string) => names.get(id) ?? t('common.playlist_singular');
	const byKey = $derived(new Map((rec.candidates ?? []).map((c) => [c.key, c])));

	let rows = $state<api.RecoverRow[] | null>(null);
	let loadError = $state<string | null>(null);
	let filter = $state<ReviewFilter>('all');
	let bulk = $state(false);

	const counts = $derived(tierCounts(rows ?? []));
	const shown = $derived(filterRows(rows ?? [], filter));
	const approved = $derived(approvedKeys(rows ?? []).length);
	const approvableMatches = $derived(approvableRows(rows ?? [], 'matched').length);
	const approvableAll = $derived(approvableRows(rows ?? []).length);

	async function load() {
		loadError = null;
		try {
			rows = await api.recoverRows(selectedKeys());
		} catch (e) {
			loadError = recoverError(e);
		}
	}

	onMount(() => {
		trackRecover();
		void load();
	});

	function put(row: api.RecoverRow) {
		if (!rows) return;
		const i = rows.findIndex((r) => r.key === row.key);
		if (i >= 0) rows[i] = row;
	}

	async function choose(row: api.RecoverRow, song: api.SongItem | null, ok: boolean) {
		try {
			put(await api.recoverPick(row.key, song, ok));
		} catch (e) {
			toast.error(recoverError(e));
		}
	}

	async function approveAll(tier?: ReviewTier) {
		if (!rows || bulk) return;
		bulk = true;
		try {
			const out = await Promise.all(
				approvableRows(rows, tier).map((r) => api.recoverPick(r.key, r.pick, true))
			);
			for (const r of out) put(r);
		} catch (e) {
			toast.error(recoverError(e));
			void load();
		} finally {
			bulk = false;
		}
	}

	// A few thousand rows at once is a stall: a page at a time, as in the pick step.
	let limit = $state(200);
	$effect(() => {
		void filter;
		limit = 200;
	});
	function more(node: HTMLElement) {
		const io = new IntersectionObserver(([e]) => e.isIntersecting && (limit += 200), {
			rootMargin: '600px 0px'
		});
		io.observe(node);
		return () => io.disconnect();
	}

	// --- the open row ----------------------------------------------------------------------------

	let open = $state<string | null>(null);
	let query = $state('');
	let results = $state<api.SongItem[]>([]);
	let searching = $state(false);
	let link = $state('');
	let linkError = $state<string | null>(null);
	let linkBusy = $state(false);
	let titleText = $state('');
	let artistText = $state('');
	/** The row searched again after a title was typed in, until its search ends. */
	let researching = $state<string | null>(null);

	function toggle(row: api.RecoverRow) {
		if (open === row.key) {
			open = null;
			return;
		}
		open = row.key;
		query = [row.title, row.artists].filter(Boolean).join(' ');
		results = [];
		link = '';
		linkError = null;
		titleText = row.title ?? '';
		artistText = row.artists ?? '';
	}

	async function search(e: Event) {
		e.preventDefault();
		if (!query.trim()) return;
		searching = true;
		try {
			results = await api.search(query.trim());
		} catch (err) {
			toast.error(String(err));
		} finally {
			searching = false;
		}
	}

	async function paste(e: Event, row: api.RecoverRow) {
		e.preventDefault();
		linkError = null;
		const id = pastedVideoId(link);
		if (!id) {
			linkError = t('recover.link_invalid');
			return;
		}
		linkBusy = true;
		try {
			const song = await api.recoverSong(id);
			if (!song) linkError = t('recover.link_not_found');
			else {
				await choose(row, song, true);
				link = '';
			}
		} catch (err) {
			linkError = recoverError(err);
		} finally {
			linkBusy = false;
		}
	}

	async function retitle(e: Event, row: api.RecoverRow) {
		e.preventDefault();
		if (researching) return;
		try {
			const named = await api.recoverSetTitle(row.key, titleText.trim(), artistText.trim() || null);
			put(named);
			if (named.title === null) return;
			researching = row.key;
			await startRun('searching', () => api.recoverSearch([row.key]));
		} catch (err) {
			researching = null;
			toast.error(recoverError(err));
		}
	}

	// The one-row search ended: read that row again.
	$effect(() => {
		const s = rec.snapshot;
		const key = researching;
		if (!key || rec.ran !== 'searching' || !s) return;
		if (s.phase !== 'done' && s.phase !== 'cancelled' && s.phase !== 'failed') return;
		untrack(() => {
			rec.ran = null;
			researching = null;
			if (s.phase === 'failed') toast.error(recoverError(s.message));
			void api
				.recoverRows([key])
				.then((r) => r.forEach(put))
				.catch(() => {});
		});
	});

	const chip = (on: boolean) =>
		`inline-flex h-8 items-center gap-1.5 rounded-full border px-3 text-xs font-medium transition-colors ${
			on ? 'border-primary/40 bg-primary/15 text-primary' : 'text-muted-foreground hover:bg-accent/10 hover:text-foreground'
		}`;
</script>

{#snippet art(url: string | null | undefined, cls: string)}
	{#if url}
		<img src={thumb(url, 96)} alt="" class="{cls} shrink-0 rounded-md object-cover" loading="lazy" />
	{:else}
		<div class="{cls} flex shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground">
			<HugeiconsIcon icon={MusicNote01Icon} class="h-4 w-4" />
		</div>
	{/if}
{/snippet}

{#snippet songLine(s: api.SongItem)}
	<div class="flex min-w-0 items-center gap-2">
		{@render art(s.thumbnail, 'h-9 w-9')}
		<div class="min-w-0">
			<div class="truncate text-sm font-medium">{s.title}</div>
			<div class="truncate text-xs text-muted-foreground">
				{[s.artists, s.album, s.duration].filter(Boolean).join(' • ')}
			</div>
		</div>
	</div>
{/snippet}

{#snippet choices(row: api.RecoverRow, list: api.SongItem[], empty: string)}
	<div class="flex flex-col">
		{#each list as c (c.video_id)}
			{@const current = row.pick?.video_id === c.video_id}
			<div class="flex items-center gap-2 rounded-lg px-2 py-1 hover:bg-accent/10 {current ? 'bg-primary/5' : ''}">
				<button
					type="button"
					class="min-w-0 flex-1 cursor-pointer text-left"
					onclick={() => choose(row, c, true)}
					title={t('recover.use_this')}
				>
					{@render songLine(c)}
				</button>
				<Button variant="ghost" size="icon-sm" title={t('recover.listen')} aria-label={t('recover.listen')} onclick={() => api.play(c)}>
					<HugeiconsIcon icon={PlayIcon} class="h-4 w-4" />
				</Button>
				{#if current}<Badge variant="label">{t('recover.current_pick')}</Badge>{/if}
			</div>
		{:else}
			<p class="px-2 py-1.5 text-xs text-muted-foreground">{empty}</p>
		{/each}
	</div>
{/snippet}

<div class="flex flex-col gap-3">
	<div class="flex flex-wrap items-center gap-2" role="group" aria-label={t('recover.tier_label')}>
		<button type="button" class={chip(filter === 'all')} aria-pressed={filter === 'all'} onclick={() => (filter = 'all')}>
			{t('recover.tier_all')}
			<span class="tabular-nums opacity-70">{rows?.length ?? 0}</span>
		</button>
		{#each REVIEW_TIERS as tier (tier)}
			{#if tier !== 'pending' || counts.pending}
				<button type="button" class={chip(filter === tier)} aria-pressed={filter === tier} onclick={() => (filter = tier)}>
					{t(`recover.tier_${tier}`)}
					<span class="tabular-nums opacity-70">{counts[tier]}</span>
				</button>
			{/if}
		{/each}
		<div class="ml-auto flex flex-wrap gap-2">
			<Button variant="outline" size="sm" onclick={() => approveAll('matched')} disabled={bulk || !approvableMatches}>
				{t('recover.approve_matches', { count: approvableMatches })}
			</Button>
			<Button variant="outline" size="sm" onclick={() => approveAll()} disabled={bulk || !approvableAll}>
				{t('recover.approve_all', { count: approvableAll })}
			</Button>
		</div>
	</div>

	{#if loadError && !rows}
		<div class="flex items-center gap-3 rounded-xl border border-destructive/30 bg-destructive/5 p-3 text-sm">
			<span class="flex-1">{loadError}</span>
			<Button variant="outline" size="sm" onclick={load}>{t('recover.retry')}</Button>
		</div>
	{:else if !rows}
		<p class="animate-pulse py-8 text-center text-sm text-muted-foreground">{t('recover.loading_rows')}</p>
	{:else if !shown.length}
		<p class="py-8 text-center text-sm text-muted-foreground">{t('recover.review_empty')}</p>
	{:else}
		<ul class="divide-y rounded-xl border">
			{#each shown.slice(0, limit) as row (row.key)}
				{@const c = byKey.get(row.key)}
				{@const where = splitKey(row.key)}
				{@const isOpen = open === row.key}
				<li class={row.approved && row.pick ? 'bg-primary/5' : ''}>
					<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -- the expand button at the end is the keyboard's way in -->
					<div class="flex cursor-pointer items-center gap-3 px-3 py-2 hover:bg-muted/40" onclick={() => toggle(row)}>
						<div class="w-[36%] min-w-0">
							<p class="truncate text-sm {row.title ? 'font-medium' : 'italic text-muted-foreground'}">
								{row.title ?? t('recover.untitled')}
							</p>
							<p class="truncate text-xs text-muted-foreground">
								{#if row.artists}{row.artists} · {/if}{nameOf(where.playlist_id)}
								· <span class="tabular-nums">{positionLabel(c?.position ?? null)}</span>
								{#if row.title_source}· {t(`recover.title_source_${row.title_source}`)}{/if}
							</p>
						</div>
						<HugeiconsIcon icon={ArrowRight02Icon} class="h-4 w-4 shrink-0 text-muted-foreground" />
						<div class="min-w-0 flex-1">
							{#if row.pick}
								{@render songLine(row.pick)}
							{:else if row.error}
								<span class="text-xs text-destructive">{recoverError(row.error)}</span>
							{:else if researching === row.key}
								<span class="animate-pulse text-xs text-muted-foreground">{t('common.searching')}</span>
							{:else if row.tier === 'pending'}
								<span class="text-xs text-muted-foreground">{t(row.title ? 'recover.not_searched' : 'recover.needs_title')}</span>
							{:else}
								<span class="text-xs text-muted-foreground">{t('recover.no_replacement')}</span>
							{/if}
						</div>
						<div class="flex shrink-0 items-center gap-1">
							{#if row.tier !== 'pending'}
								<Badge variant={row.tier === 'matched' ? 'label' : 'muted'}>{t(`recover.tier_${row.tier}`)}</Badge>
							{/if}
							{#if row.pick}
								{@const pick = row.pick}
								<Button
									variant="ghost"
									size="icon-sm"
									title={t('recover.listen')}
									aria-label={t('recover.listen')}
									onclick={(e) => {
										e.stopPropagation();
										api.play(pick);
									}}
								>
									<HugeiconsIcon icon={PlayIcon} class="h-4 w-4" />
								</Button>
								<Button
									variant={row.approved ? 'default' : 'outline'}
									size="icon-sm"
									title={t('recover.approve')}
									aria-label={t('recover.approve')}
									aria-pressed={row.approved}
									onclick={(e) => {
										e.stopPropagation();
										void choose(row, pick, true);
									}}
								>
									<HugeiconsIcon icon={Tick02Icon} class="h-4 w-4" />
								</Button>
								<Button
									variant="ghost"
									size="icon-sm"
									title={t('recover.reject')}
									aria-label={t('recover.reject')}
									disabled={!row.approved}
									onclick={(e) => {
										e.stopPropagation();
										void choose(row, pick, false);
									}}
								>
									<HugeiconsIcon icon={Cancel01Icon} class="h-4 w-4" />
								</Button>
							{/if}
							<Button
								variant="ghost"
								size="icon-sm"
								title={t(isOpen ? 'recover.collapse' : 'recover.expand')}
								aria-label={t(isOpen ? 'recover.collapse' : 'recover.expand')}
								aria-expanded={isOpen}
								onclick={(e) => {
									e.stopPropagation();
									toggle(row);
								}}
							>
								<HugeiconsIcon icon={isOpen ? ArrowUp01Icon : ArrowDown01Icon} class="h-4 w-4" />
							</Button>
						</div>
					</div>

					{#if isOpen}
						<div class="grid gap-4 border-t bg-muted/20 px-3 py-3 md:grid-cols-2">
							<div class="flex min-w-0 flex-col gap-2">
								<p class="text-xs font-medium text-muted-foreground">{t('recover.other_candidates')}</p>
								{@render choices(row, row.candidates, t('recover.no_candidates'))}
								{#if row.pick}
									<div>
										<Button variant="ghost" size="sm" onclick={() => choose(row, null, false)}>
											{t('recover.no_replacement_action')}
										</Button>
									</div>
								{/if}
							</div>
							<div class="flex min-w-0 flex-col gap-3">
								<form class="flex flex-col gap-1.5" onsubmit={search}>
									<span class="text-xs font-medium text-muted-foreground">{t('recover.manual_search')}</span>
									<div class="flex gap-2">
										<Input bind:value={query} class="h-8" aria-label={t('recover.manual_search')} />
										<Button type="submit" size="icon-sm" variant="outline" aria-label={t('recover.manual_search')} disabled={searching}>
											<HugeiconsIcon icon={Search01Icon} class="h-4 w-4" />
										</Button>
									</div>
								</form>
								{#if searching || results.length}
									<div class="max-h-64 overflow-y-auto">
										{@render choices(row, results, searching ? t('common.searching') : t('recover.no_candidates'))}
									</div>
								{/if}
								<form class="flex flex-col gap-1.5" onsubmit={(e) => paste(e, row)}>
									<span class="flex items-center gap-1 text-xs font-medium text-muted-foreground">
										<HugeiconsIcon icon={Link01Icon} class="h-3.5 w-3.5" />
										{t('recover.paste_link')}
									</span>
									<div class="flex gap-2">
										<Input bind:value={link} class="h-8" placeholder="https://music.youtube.com/watch?v=…" aria-label={t('recover.paste_link')} />
										<Button type="submit" size="sm" variant="outline" disabled={linkBusy || !link.trim()}>{t('recover.use_link')}</Button>
									</div>
									{#if linkError}<span class="text-xs text-destructive">{linkError}</span>{/if}
								</form>
								<form class="flex flex-col gap-1.5" onsubmit={(e) => retitle(e, row)}>
									<span class="text-xs font-medium text-muted-foreground">
										{t(row.title ? 'recover.retitle' : 'recover.name_it')}
									</span>
									<div class="flex flex-wrap gap-2">
										<Input bind:value={titleText} class="h-8 min-w-40 flex-1" placeholder={t('recover.title_placeholder')} aria-label={t('recover.title_placeholder')} />
										<Input bind:value={artistText} class="h-8 w-40" placeholder={t('recover.artist_placeholder')} aria-label={t('recover.artist_placeholder')} />
										<Button type="submit" size="sm" variant="outline" disabled={!!researching || !titleText.trim()}>
											{t('recover.search_again')}
										</Button>
									</div>
								</form>
							</div>
						</div>
					{/if}
				</li>
			{/each}
		</ul>
		{#if shown.length > limit}
			<div {@attach more} class="h-px"></div>
		{/if}
	{/if}

	<div class="sticky bottom-0 flex items-center gap-3 border-t bg-background/95 py-3 backdrop-blur">
		<Button variant="outline" size="sm" onclick={onback}>
			<HugeiconsIcon icon={ArrowLeft01Icon} class="h-4 w-4" />
			{t('recover.back')}
		</Button>
		<span class="text-sm text-muted-foreground">{t('recover.approved_count', { approved, total: rows?.length ?? 0 })}</span>
		<Button class="ml-auto" onclick={onnext} disabled={!approved}>
			{t('recover.next')}
			<HugeiconsIcon icon={ArrowRight01Icon} class="h-4 w-4" />
		</Button>
	</div>
</div>
