<script lang="ts">
	// Every copy of one song across your playlists ("+N" in Library ▸ In your playlists), and what to
	// do with the ones you tick: remove them, move them to a playlist, or move them to a new one
	// (PlaylistForge's duplicate-occurrences modal). The list comes from the index and the snapshots
	// (`song_occurrences`), so it shows positions as last seen; the writes read each playlist fresh
	// and find the ticked copies again there (`occurrences.ts`), since stored handles go stale.
	import * as Dialog from '$lib/components/ui/dialog';
	import { Button } from '$lib/components/ui/button';
	import { Checkbox } from '$lib/components/ui/checkbox';
	import { Input } from '$lib/components/ui/input';
	import * as RadioGroup from '$lib/components/ui/radio-group';
	import PlaylistSelect from './PlaylistSelect.svelte';
	import EngineChoice from './ytdata/EngineChoice.svelte';
	import * as api from '$lib/api';
	import type { Occurrence, PlaylistOp, SongItem } from '$lib/api';
	import { occKey, planOccurrences, resolveRows, type OccurrenceAction } from '$lib/occurrences';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { auth, bumpLibraryTrackCount, createLibraryPlaylist, library, personal, toast } from '$lib/player.svelte';
	import { announceOps } from '$lib/playlistops.svelte';
	import { canDropOn } from '$lib/transfer.svelte';
	import type { PlaylistEngine } from '$lib/ytdata.svelte';
	import { t } from '$lib/i18n.svelte';

	let {
		open = $bindable(false),
		song,
		occurrences,
		nameOf,
		onapplied
	}: {
		open: boolean;
		/** The song whose copies these are. */
		song: SongItem | null;
		occurrences: Occurrence[];
		nameOf: (id: string) => string;
		/** After a write went through (the dialog has closed by then). */
		onapplied?: () => void;
	} = $props();

	const ACTIONS: OccurrenceAction[] = ['remove', 'move', 'move_new'];

	let selected = $state<Set<string>>(new Set());
	let action = $state<OccurrenceAction>('remove');
	let destId = $state('');
	let newName = $state('');
	/** This write's engine, once the user picks one; unset follows `playlist_engine`. */
	let engine = $state<PlaylistEngine | undefined>();
	let busy = $state(false);

	// Each opening starts clean: nothing ticked (acting on a copy is a choice, never a default).
	$effect(() => {
		if (!open) return;
		selected = new Set();
		action = 'remove';
		destId = '';
		newName = '';
		engine = undefined;
	});

	const playlistCount = $derived(new Set(occurrences.map((o) => o.playlist_id)).size);
	const targets = $derived(
		orderLibrary(mergeSaved(personal, library.items, 'playlist'), personal)
			.filter((p) => canDropOn(p, null))
			.map((p) => ({ value: p.id, label: p.title }))
	);
	const plan = $derived(planOccurrences(occurrences, selected, action, action === 'move' ? destId || null : null));
	const ready = $derived(plan.count > 0 && (action !== 'move_new' || !!newName.trim()));
	const priced = $derived([
		{
			kind: action === 'remove' ? ('remove' as const) : ('move' as const),
			rows: plan.count,
			playlists: [...plan.byPlaylist.keys(), ...(action === 'move' && destId ? [destId] : [])]
		}
	]);

	function toggle(o: Occurrence, on: boolean) {
		const next = new Set(selected);
		if (on) next.add(occKey(o));
		else next.delete(occKey(o));
		selected = next;
	}

	async function apply() {
		if (busy || !song || !ready) return;
		// Read once: the dialog closes before the toast, and the choices must not move under the loop.
		const videoId = song.video_id;
		const [what, groups, choice, name] = [action, plan.byPlaylist, engine, newName.trim()];
		busy = true;
		const ops: (PlaylistOp | null)[] = [];
		let done = 0;
		let missing = 0;
		let failed = 0;
		let target: { id: string; title: string } | null = null;
		let created = false;
		const summary = () => {
			const main =
				what === 'remove'
					? done === 1
						? t('occurrences.removed_one')
						: t('occurrences.removed', { count: done })
					: done === 1
						? t('occurrences.moved_one', { playlist: target?.title ?? '' })
						: t('occurrences.moved', { count: done, playlist: target?.title ?? '' });
			const notes = [
				missing ? t('occurrences.missing', { count: missing }) : '',
				failed ? t('occurrences.failed', { count: failed }) : '',
				// Made outside the journal (D6): an undo puts the tracks back but keeps the playlist.
				created && target ? t('occurrences.new_kept', { playlist: target.title }) : ''
			].filter(Boolean);
			return [main, ...notes].join(' · ');
		};
		try {
			if (what === 'move') {
				target = { id: destId, title: targets.find((p) => p.value === destId)?.label ?? nameOf(destId) };
			} else if (what === 'move_new') {
				const item = await createLibraryPlaylist(name, !auth.account?.signedIn);
				target = { id: item.id, title: item.title };
				created = true;
			}
			for (const [pid, picked] of groups) {
				let rows: api.RowRef[];
				try {
					rows = resolveRows(await api.playlistRows(pid), videoId, picked);
				} catch {
					failed++;
					continue;
				}
				missing += picked.length - rows.length;
				if (!rows.length) continue;
				const source = { id: pid, title: nameOf(pid) };
				try {
					if (target) {
						const r = await api.transferTracks({
							source,
							target,
							rows,
							mode: 'move',
							duplicates: 'consolidate',
							engine: choice
						});
						ops.push(r.op);
						done += r.removed;
						if (r.added) bumpLibraryTrackCount(target.id, r.added);
						if (r.removed) bumpLibraryTrackCount(pid, -r.removed);
					} else {
						ops.push(await api.removeTracks(pid, source.title, rows, 'remove', choice));
						done += rows.length;
						bumpLibraryTrackCount(pid, -rows.length);
					}
				} catch (e) {
					// A cooldown stops the whole run: every playlist after this one would hit it too.
					if (String(e).startsWith('cooldown:')) throw e;
					failed++;
				}
			}
			open = false;
			announceOps(ops, summary());
			onapplied?.();
		} catch (e) {
			// What already went through can still be undone.
			if (ops.length) announceOps(ops, summary());
			toast.error(String(e));
		} finally {
			busy = false;
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content class="sm:max-w-lg">
		<Dialog.Header>
			<Dialog.Title>
				{playlistCount === 1 ? t('occurrences.title_one') : t('occurrences.title', { count: playlistCount })}
			</Dialog.Title>
			{#if song}
				<Dialog.Description class="truncate">
					<span class="font-medium text-foreground">{song.title}</span>
					{#if song.artists}<span> · {song.artists}</span>{/if}
				</Dialog.Description>
			{/if}
		</Dialog.Header>
		<p class="text-xs text-muted-foreground">{t('occurrences.desc')}</p>
		<div class="max-h-64 overflow-y-auto rounded-xl border p-1" role="list" aria-label={t('occurrences.list')}>
			{#each occurrences as o (occKey(o))}
				{@const name = nameOf(o.playlist_id)}
				<div role="listitem" class="flex items-center gap-2 rounded-lg px-2 py-1.5 hover:bg-accent/10">
					<Checkbox
						checked={selected.has(occKey(o))}
						onCheckedChange={(v) => toggle(o, !!v)}
						aria-label={t('occurrences.select', { playlist: name })}
					/>
					<a
						href="/playlist/{encodeURIComponent(o.playlist_id)}"
						class="min-w-0 flex-1 truncate text-sm hover:underline"
						onclick={() => (open = false)}
					>
						{name}
					</a>
					<span class="shrink-0 text-xs text-muted-foreground tabular-nums">
						{o.position === null
							? t('occurrences.position_unknown')
							: t('occurrences.position', { position: o.position + 1 })}
					</span>
				</div>
			{:else}
				<p class="px-2 py-4 text-center text-xs text-muted-foreground">{t('occurrences.empty')}</p>
			{/each}
		</div>
		<div>
			<p class="text-xs font-medium text-muted-foreground">{t('occurrences.action')}</p>
			<RadioGroup.Root
				value={action}
				onValueChange={(v) => (action = v as OccurrenceAction)}
				class="mt-1 gap-0"
			>
				{#each ACTIONS as key (key)}
					<label class="flex cursor-pointer items-center gap-2 rounded-md px-1 py-1 text-sm hover:bg-accent/10">
						<RadioGroup.Item value={key} />
						{t(`occurrences.action_${key}`)}
					</label>
				{/each}
			</RadioGroup.Root>
			{#if action === 'move'}
				<PlaylistSelect
					class="mt-2 w-full"
					value={destId}
					options={targets}
					onpick={(v) => (destId = v)}
					placeholder={t('occurrences.dest_placeholder')}
					label={t('occurrences.dest')}
				/>
				{#if plan.skipped}
					<p class="mt-1.5 text-xs text-amber-500">
						{plan.skipped === 1
							? t('occurrences.skipped_one')
							: t('occurrences.skipped', { count: plan.skipped })}
					</p>
				{/if}
			{:else if action === 'move_new'}
				<Input
					class="mt-2"
					bind:value={newName}
					placeholder={t('occurrences.new_name')}
					aria-label={t('occurrences.new_name')}
				/>
			{/if}
			<EngineChoice bind:choice={engine} ops={priced} />
		</div>
		<Dialog.Footer>
			<Button type="button" variant="outline" onclick={() => (open = false)}>{t('common.cancel')}</Button>
			<Button type="button" disabled={busy || !ready} onclick={apply}>
				{busy
					? t('common.loading')
					: plan.count === 1
						? t('occurrences.apply_one')
						: t('occurrences.apply', { count: plan.count })}
			</Button>
		</Dialog.Footer>
	</Dialog.Content>
</Dialog.Root>
