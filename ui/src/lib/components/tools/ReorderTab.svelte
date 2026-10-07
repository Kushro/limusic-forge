<script lang="ts">
	// Save an order to a playlist (PlaylistForge's reorder wizard): the playlist page's sort choices
	// (`PlaylistSortPanel`), here applied to the whole playlist and written as its own order. The
	// backend moves only the rows that have to move (`playlist_tools/lis.rs`), and the toast offers
	// the undo.
	import * as api from '$lib/api';
	import type { SongItem } from '$lib/api';
	import { toast } from '$lib/player.svelte';
	import { announceOp } from '$lib/playlistops.svelte';
	import { movedCount } from '$lib/reorder';
	import { sortSongs, type SortKey } from '$lib/sort';
	import { t } from '$lib/i18n.svelte';
	import { Switch } from '../ui/switch';
	import PlaylistSortPanel from '../PlaylistSortPanel.svelte';

	let {
		playlistId,
		title,
		editable,
		ondone
	}: { playlistId: string; title: string; editable: boolean; ondone: () => void } = $props();

	let rows = $state.raw<SongItem[] | null>(null);
	let loadError = $state<string | null>(null);
	let sort = $state<SortKey>('default');
	let desc = $state(false);
	let plays = $state<Record<string, number>>({});
	let saving = $state(false);

	const handleOf = (s: SongItem) => s.set_video_id ?? '';

	$effect(() => {
		const pid = playlistId;
		rows = null;
		loadError = null;
		api
			.playlistRows(pid)
			.then((r) => {
				if (pid === playlistId) rows = r;
			})
			.catch((e) => {
				if (pid === playlistId) loadError = String(e);
			});
	});

	function choose(key: SortKey) {
		sort = key;
		// The listening history is only read if "Most played" is picked; without it everything
		// counts as unplayed, which keeps the playlist's order.
		if (key === 'plays')
			api
				.getPlayCounts()
				.then((c) => void (plays = c))
				.catch(() => {});
	}

	// Liked Music is the one list that arrives newest first, and it can't be rearranged anyway.
	const target = $derived(rows ? sortSongs(rows, sort, false, desc, plays) : []);
	const moving = $derived(rows ? movedCount(rows.map(handleOf), target.map(handleOf)) : 0);

	async function save() {
		if (!rows || saving || !moving) return;
		if (target.some((s) => !s.set_video_id)) {
			toast.error(t('reorder.unconfirmed'));
			return;
		}
		saving = true;
		try {
			const op = await api.reorderPlaylist(playlistId, title, target.map(handleOf));
			announceOp(op, moving === 1 ? t('reorder.applied_one') : t('reorder.applied', { count: moving }));
			ondone();
		} catch (e) {
			toast.error(t('reorder.failed', { error: String(e) }));
		} finally {
			saving = false;
		}
	}
</script>

<div class="space-y-4">
	<p class="text-sm text-muted-foreground">{t('reorder.tool_intro', { playlist: title })}</p>
	{#if !editable}
		<p class="text-sm text-muted-foreground" role="status">{t('reorder.tool_not_editable')}</p>
	{:else if loadError}
		<p class="text-sm text-destructive" role="alert">{loadError}</p>
	{:else if !rows}
		<p class="text-sm text-muted-foreground" role="status">{t('extract.loading')}</p>
	{:else}
		<div class="rounded-lg border p-1">
			<PlaylistSortPanel
				value={sort}
				onchoose={choose}
				canKeep={moving > 0}
				onkeep={save}
				{saving}
			/>
		</div>
		<label class="flex items-center gap-2 text-sm">
			<Switch bind:checked={desc} />
			{t('reorder.tool_reverse')}
		</label>
		<p class="text-sm" role="status">
			{moving === 0
				? t('reorder.tool_in_order')
				: moving === 1
					? t('reorder.tool_will_move_one')
					: t('reorder.tool_will_move', { count: moving })}
		</p>
	{/if}
</div>
