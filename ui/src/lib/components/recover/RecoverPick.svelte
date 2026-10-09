<script lang="ts">
	// Recover tracks, step 1 (F4.2): every dead track of your playlists, once per playlist, from the
	// three sources. Filters narrow the list; the selection (kept in `rec`, so it outlives the page)
	// is what the later steps work on. A row click toggles it, Shift+click adds the range from the
	// last one clicked; a selected row filtered out stays selected.
	import { onMount, tick } from 'svelte';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { ArrowRight01Icon, MusicNote01Icon, Search01Icon } from '@hugeicons/core-free-icons';
	import type { RecoverSource } from '$lib/api';
	import { candidatePlaylists, positionLabel, RECOVER_SOURCES, sourceCounts } from '$lib/recover';
	import {
		clearSelection,
		clickRow,
		loadCandidates,
		rec,
		selectAll,
		selectShown,
		setFilter,
		shownCandidates
	} from '$lib/recover.svelte';
	import { library, personal } from '$lib/player.svelte';
	import { mergeSaved } from '$lib/personal';
	import { thumb } from '$lib/thumb';
	import { t } from '$lib/i18n.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Checkbox } from '$lib/components/ui/checkbox';
	import { Input } from '$lib/components/ui/input';
	import { Switch } from '$lib/components/ui/switch';
	import PlaylistSelect from '$lib/components/PlaylistSelect.svelte';

	let { onnext }: { onnext: () => void } = $props();

	const names = $derived(
		new Map(mergeSaved(personal, library.items, 'playlist').map((p) => [p.id, p.title]))
	);
	const nameOf = (id: string) => names.get(id) ?? t('common.playlist_singular');

	// Counted over what the dismissed switch lets through, the other filters aside.
	const listed = $derived((rec.candidates ?? []).filter((c) => rec.filter.includeDismissed || !c.dismissed));
	const counts = $derived(sourceCounts(listed));
	const playlistOptions = $derived([
		{ value: '', label: t('recover.all_playlists') },
		...candidatePlaylists(listed)
			.map((id) => ({ value: id, label: nameOf(id) }))
			.sort((a, b) => a.label.localeCompare(b.label))
	]);
	const shown = $derived(shownCandidates());

	// A few thousand rows at once is a stall: a page at a time, as Library ▸ In your playlists.
	let limit = $state(200);
	$effect(() => {
		void rec.filter;
		limit = 200;
	});
	function more(node: HTMLElement) {
		const io = new IntersectionObserver(([e]) => e.isIntersecting && (limit += 200), {
			rootMargin: '600px 0px'
		});
		io.observe(node);
		return () => io.disconnect();
	}

	// A preselected row (from an alert) comes into view once, however far down it is.
	$effect(() => {
		const key = rec.focus;
		if (!key) return;
		const i = shown.findIndex((c) => c.key === key);
		if (i < 0) return;
		if (i >= limit) limit = i + 50;
		rec.focus = null;
		void tick().then(() =>
			[...document.querySelectorAll<HTMLElement>('[data-recover-key]')]
				.find((el) => el.dataset.recoverKey === key)
				?.scrollIntoView({ block: 'center' })
		);
	});

	onMount(() => {
		void loadCandidates();
	});

	function toggleSource(s: RecoverSource) {
		const on = rec.filter.sources.includes(s);
		setFilter({ sources: on ? rec.filter.sources.filter((x) => x !== s) : [...rec.filter.sources, s] });
	}

	const chip = (on: boolean) =>
		`inline-flex h-8 items-center gap-1.5 rounded-full border px-3 text-xs font-medium transition-colors ${
			on ? 'border-primary/40 bg-primary/15 text-primary' : 'text-muted-foreground hover:bg-accent/10 hover:text-foreground'
		}`;
</script>

<div class="flex flex-col gap-3">
	<div class="flex flex-wrap items-center gap-2" role="group" aria-label={t('recover.sources_label')}>
		{#each RECOVER_SOURCES as s (s)}
			<button type="button" class={chip(rec.filter.sources.includes(s))} aria-pressed={rec.filter.sources.includes(s)} onclick={() => toggleSource(s)}>
				{t(`recover.source_${s}`)}
				<span class="tabular-nums opacity-70">{counts[s]}</span>
			</button>
		{/each}
	</div>

	<div class="flex flex-wrap items-center gap-3">
		<PlaylistSelect
			value={rec.filter.playlist}
			options={playlistOptions}
			onpick={(v) => setFilter({ playlist: v })}
			placeholder={t('recover.all_playlists')}
			label={t('recover.playlist_filter')}
			class="w-64 max-w-full"
		/>
		<div class="relative w-72 max-w-full">
			<HugeiconsIcon icon={Search01Icon} class="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
			<Input
				value={rec.filter.text}
				oninput={(e) => setFilter({ text: e.currentTarget.value })}
				placeholder={t('recover.search')}
				aria-label={t('recover.search')}
				class="pl-9"
			/>
		</div>
		<label class="flex items-center gap-2 text-sm">
			<Switch checked={rec.filter.includeDismissed} onCheckedChange={(v) => setFilter({ includeDismissed: v })} />
			{t('recover.include_dismissed')}
		</label>
	</div>

	<div class="flex flex-wrap items-center gap-2">
		<Button variant="outline" size="sm" onclick={selectAll} disabled={!listed.length}>{t('recover.select_all')}</Button>
		<Button variant="outline" size="sm" onclick={clearSelection} disabled={!rec.selected.size}>{t('recover.select_none')}</Button>
		<Button variant="outline" size="sm" onclick={selectShown} disabled={!shown.length}>
			{t('recover.select_filtered', { count: shown.length })}
		</Button>
		<span class="text-xs text-muted-foreground">
			{t('recover.shown', { shown: shown.length, total: listed.length })} · {t('recover.shift_hint')}
		</span>
	</div>

	{#if rec.preselectMissing}
		<p class="rounded-md border border-border bg-muted/40 px-3 py-2 text-sm text-muted-foreground">{t('recover.preselect_missing')}</p>
	{/if}

	{#if rec.error && !rec.candidates}
		<div class="flex items-center gap-3 rounded-xl border border-destructive/30 bg-destructive/5 p-3 text-sm">
			<span class="flex-1">{t('recover.error', { error: rec.error })}</span>
			<Button variant="outline" size="sm" onclick={() => loadCandidates()}>{t('recover.retry')}</Button>
		</div>
	{:else if !rec.candidates}
		<p class="animate-pulse py-8 text-center text-sm text-muted-foreground">{t('recover.loading')}</p>
	{:else if !listed.length}
		<p class="py-8 text-center text-sm text-muted-foreground">{t('recover.empty')}</p>
	{:else if !shown.length}
		<p class="py-8 text-center text-sm text-muted-foreground">{t('recover.none_match')}</p>
	{:else}
		<ul class="divide-y rounded-xl border">
			{#each shown.slice(0, limit) as c (c.key)}
				{@const on = rec.selected.has(c.key)}
				<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -- the row's click mirrors its checkbox, which the keyboard reaches -->
				<li
					data-recover-key={c.key}
					class="flex cursor-pointer items-center gap-3 px-3 py-2 select-none hover:bg-muted/40 {on ? 'bg-primary/5' : ''} {c.dismissed ? 'opacity-60' : ''}"
					onclick={(e) => clickRow(c.key, e.shiftKey)}
				>
					<Checkbox
						bind:checked={() => on, () => {}}
						aria-label={c.title ?? c.video_id}
					/>
					{#if c.thumbnail}
						<img src={thumb(c.thumbnail, 40)} alt="" loading="lazy" class="h-10 w-10 shrink-0 rounded object-cover" />
					{:else}
						<div class="flex h-10 w-10 shrink-0 items-center justify-center rounded bg-muted">
							<HugeiconsIcon icon={MusicNote01Icon} class="h-4 w-4 text-muted-foreground" />
						</div>
					{/if}
					<div class="min-w-0 flex-1">
						<p class="truncate text-sm {c.title ? '' : 'italic text-muted-foreground'}">
							{c.title ?? t('recover.untitled')}
							{#if c.artists}<span class="not-italic text-muted-foreground"> · {c.artists}</span>{/if}
						</p>
						<p class="truncate text-xs text-muted-foreground">
							<span class="font-mono">{c.video_id}</span>
							· {nameOf(c.playlist_id)}
							· <span title={t('recover.position_hint')} class="tabular-nums">{positionLabel(c.position)}</span>
							{#if !c.in_playlist}· {t('recover.not_in_playlist')}{/if}
						</p>
					</div>
					<div class="flex shrink-0 flex-wrap justify-end gap-1">
						{#if c.dismissed}<Badge variant="outline">{t('recover.dismissed')}</Badge>{/if}
						{#each c.sources as s (s)}
							<Badge variant={s === 'alert_removed' ? 'muted' : 'label'}>{t(`recover.source_${s}`)}</Badge>
						{/each}
					</div>
				</li>
			{/each}
		</ul>
		{#if shown.length > limit}
			<div {@attach more} class="h-px"></div>
		{/if}
	{/if}

	<div class="sticky bottom-0 flex items-center gap-3 border-t bg-background/95 py-3 backdrop-blur">
		<span class="text-sm text-muted-foreground">{t('recover.selected', { count: rec.selected.size })}</span>
		<Button class="ml-auto" onclick={onnext} disabled={!rec.selected.size}>
			{t('recover.next')}
			<HugeiconsIcon icon={ArrowRight01Icon} class="h-4 w-4" />
		</Button>
	</div>
</div>
