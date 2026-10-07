<script lang="ts">
	// Merge this playlist with others (`playlist_tools/merge.rs`): into a new playlist, or appended
	// to this one when it is yours. Duplicates left out by default; one list after another, or one
	// track from each in turn. The sources are never changed.
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { ArrowLeft01Icon, GitMergeIcon } from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { SongItem } from '$lib/api';
	import { auth, library, personal, toast } from '$lib/player.svelte';
	import { mergeSaved, orderLibrary } from '$lib/personal';
	import { building, runBuild, stopBuild } from '$lib/build.svelte';
	import { t } from '$lib/i18n.svelte';
	import { Button } from '../ui/button';
	import { Checkbox } from '../ui/checkbox';
	import { Input } from '../ui/input';
	import { Switch } from '../ui/switch';
	import * as RadioGroup from '../ui/radio-group';
	import BuildProgress from './BuildProgress.svelte';

	let {
		playlistId,
		title,
		editable,
		ondone
	}: { playlistId: string; title: string; editable: boolean; ondone: () => void } = $props();

	let filter = $state('');
	let picked = $state<string[]>([]);
	let dedupe = $state(true);
	let how = $state<'concat' | 'round_robin'>('concat');
	let into = $state<'new' | 'this'>('new');
	let name = $state('');
	let previewing = $state(false);
	let merged = $state.raw<SongItem[] | null>(null);
	const signedIn = $derived(!!auth.account?.signedIn);
	let local = $state(false);
	$effect(() => {
		local = api.isLocalPlaylist(playlistId) || !signedIn;
	});

	const others = $derived(
		orderLibrary(mergeSaved(personal, library.items, 'playlist'), personal).filter(
			(p) => p.kind === 'playlist' && p.id !== playlistId && p.id !== api.ON_REPEAT_ID
		)
	);
	const matches = $derived(
		others.filter((p) => p.title.toLowerCase().includes(filter.trim().toLowerCase()))
	);
	const pickedItems = $derived(others.filter((p) => picked.includes(p.id)));

	function toggle(id: string, on: boolean) {
		picked = on ? [...picked, id] : picked.filter((p) => p !== id);
		merged = null;
	}

	async function preview() {
		if (!picked.length || previewing) return;
		previewing = true;
		try {
			const ids = [playlistId, ...pickedItems.map((p) => p.id)];
			// Appending to this playlist: only the others' tracks go in, and what it has is skipped.
			merged = await api.mergePreview(
				into === 'this' ? ids.slice(1) : ids,
				how,
				dedupe,
				into === 'this' ? playlistId : null
			);
			if (!name.trim()) name = [title, ...pickedItems.map((p) => p.title)].join(' + ');
		} catch (e) {
			toast.error(String(e));
		} finally {
			previewing = false;
		}
	}

	async function create() {
		if (!merged?.length) return;
		const sources = [{ id: playlistId, title }, ...pickedItems.map((p) => ({ id: p.id, title: p.title }))];
		const target = into === 'this' ? title : name.trim() || title;
		const b = await runBuild(
			{
				kind: 'merge',
				sources,
				lists: [{ name: target, songs: merged }],
				dest: into === 'this' ? { to: 'existing', id: playlistId, title } : { to: 'new', local }
			},
			(b) => t('merge.done', { count: b.added, playlist: target })
		);
		if (b && !b.error) ondone();
	}
</script>

{#if building.running}
	<BuildProgress onstop={stopBuild} />
{:else if !merged}
	<div class="space-y-4">
		<p class="text-sm text-muted-foreground">{t('merge.intro', { playlist: title })}</p>
		<Input bind:value={filter} placeholder={t('drop.search')} aria-label={t('drop.search')} />
		<div class="max-h-48 space-y-0.5 overflow-y-auto rounded-lg border p-1">
			{#each matches as p (p.id)}
				<label class="flex cursor-pointer items-center gap-3 rounded-md px-2 py-1.5 text-sm hover:bg-accent/10">
					<Checkbox checked={picked.includes(p.id)} onCheckedChange={(v) => toggle(p.id, !!v)} />
					<span class="min-w-0 flex-1 truncate">{p.title}</span>
				</label>
			{:else}
				<p class="px-2 py-3 text-sm text-muted-foreground">{t('merge.no_others')}</p>
			{/each}
		</div>
		<div class="grid gap-3 sm:grid-cols-2">
			<div>
				<p class="mb-1 text-xs font-medium text-muted-foreground">{t('merge.order')}</p>
				<RadioGroup.Root value={how} onValueChange={(v) => (how = v as typeof how)} class="gap-1">
					<label class="flex cursor-pointer items-center gap-2 text-sm">
						<RadioGroup.Item value="concat" />{t('merge.concat')}
					</label>
					<label class="flex cursor-pointer items-center gap-2 text-sm">
						<RadioGroup.Item value="round_robin" />{t('merge.round_robin')}
					</label>
				</RadioGroup.Root>
			</div>
			<div>
				<p class="mb-1 text-xs font-medium text-muted-foreground">{t('merge.into')}</p>
				<RadioGroup.Root value={into} onValueChange={(v) => (into = v as typeof into)} class="gap-1">
					<label class="flex cursor-pointer items-center gap-2 text-sm">
						<RadioGroup.Item value="new" />{t('merge.into_new')}
					</label>
					<label class="flex cursor-pointer items-center gap-2 text-sm {editable ? '' : 'opacity-50'}">
						<RadioGroup.Item value="this" disabled={!editable} />{t('merge.into_this')}
					</label>
				</RadioGroup.Root>
			</div>
		</div>
		<label class="flex items-center gap-2 text-sm">
			<Switch bind:checked={dedupe} />
			{t('merge.dedupe')}
		</label>
		<div class="flex justify-end">
			<Button onclick={preview} disabled={!picked.length || previewing} class="gap-2">
				<HugeiconsIcon icon={GitMergeIcon} class="h-4 w-4" />
				{previewing ? t('common.loading') : t('merge.preview')}
			</Button>
		</div>
	</div>
{:else}
	<div class="flex min-h-0 flex-col gap-3">
		<div class="flex items-center gap-2">
			<Button variant="ghost" size="icon-sm" onclick={() => (merged = null)} aria-label={t('common.back')}>
				<HugeiconsIcon icon={ArrowLeft01Icon} class="h-4 w-4" />
			</Button>
			<p class="text-sm" role="status">
				{merged.length === 1 ? t('merge.summary_one') : t('merge.summary', { count: merged.length })}
			</p>
		</div>
		<ol class="max-h-[35vh] list-decimal space-y-0.5 overflow-y-auto pl-8 pr-1 text-sm">
			{#each merged.slice(0, 200) as s, i (i)}
				<li class="truncate"><span>{s.title}</span> <span class="text-muted-foreground">· {s.artists}</span></li>
			{/each}
		</ol>
		{#if merged.length > 200}
			<p class="text-xs text-muted-foreground">{t('merge.more', { count: merged.length - 200 })}</p>
		{/if}
		{#if into === 'new'}
			<Input bind:value={name} aria-label={t('split.name')} />
			{#if signedIn && !api.isLocalPlaylist(playlistId)}
				<label class="flex items-center gap-2 text-sm">
					<Switch bind:checked={local} />
					{t('build.keep_local')}
				</label>
			{/if}
		{/if}
		<div class="flex justify-end">
			<Button onclick={create} disabled={!merged.length}>
				{into === 'this'
					? t('merge.add_here', { count: merged.length })
					: t('merge.create')}
			</Button>
		</div>
	</div>
{/if}
