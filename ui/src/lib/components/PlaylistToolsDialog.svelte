<script lang="ts" module>
	export type ToolTab = 'duplicates' | 'split' | 'merge' | 'extract' | 'reorder';
</script>

<script lang="ts">
	// Playlist tools (`playlist_tools/`), one dialog with a tab per tool, opened from a playlist's
	// ⋯ menu or from the Tools page (`routes/tools`). Each tab is the same three beats
	// PlaylistForge's wizards have: set it up, review what it will do (and untick what you don't
	// want), then do it, with the toast offering an undo.
	import * as Dialog from '$lib/components/ui/dialog';
	import * as Tabs from '$lib/components/ui/tabs';
	import DedupTab from './tools/DedupTab.svelte';
	import SplitTab from './tools/SplitTab.svelte';
	import MergeTab from './tools/MergeTab.svelte';
	import ExtractTab from './tools/ExtractTab.svelte';
	import ReorderTab from './tools/ReorderTab.svelte';
	import EngineChoice from './ytdata/EngineChoice.svelte';
	import { isLocalPlaylist, setEngineChoice } from '$lib/api';
	import type { PlaylistEngine } from '$lib/ytdata.svelte';
	import { t } from '$lib/i18n.svelte';

	let {
		open = $bindable(false),
		tab = $bindable<ToolTab>('duplicates'),
		playlistId,
		title,
		editable
	}: {
		open: boolean;
		tab?: ToolTab;
		playlistId: string;
		title: string;
		editable: boolean;
	} = $props();

	// The engine for the tabs whose writes take one (removing duplicates; extracting by copy or
	// move): picked here, priced per track since the rows are chosen inside the tab, and handed to
	// those writes for as long as the dialog is open on them (`api.setEngineChoice`). Splits, merges
	// and builds into a new playlist go their own way. A playlist on this computer has no engine.
	const ENGINE_TABS: ToolTab[] = ['duplicates', 'extract'];
	let engine = $state<PlaylistEngine | undefined>();
	const engineTab = $derived(editable && ENGINE_TABS.includes(tab) && !isLocalPlaylist(playlistId));
	const priced = $derived([
		{ kind: tab === 'duplicates' ? ('remove' as const) : ('copy' as const), rows: 1, playlists: [playlistId] }
	]);
	$effect(() => {
		if (!open) engine = undefined;
	});
	$effect(() => {
		if (!open || !engineTab || engine === undefined) return;
		setEngineChoice(engine);
		return () => setEngineChoice(null);
	});
</script>

<Dialog.Root bind:open>
	<Dialog.Content class="sm:max-w-2xl">
		<Dialog.Header>
			<Dialog.Title>{t('tools.title')}</Dialog.Title>
			<Dialog.Description class="truncate">{title}</Dialog.Description>
		</Dialog.Header>
		<!-- Mounted per opening: a tool starts over each time, not on the last preview. A split or
		     a merge still writing carries on with the dialog closed (`build.svelte.ts`). -->
		{#if open}
			<!-- min-w-0: the dialog is a grid, so a long unwrapped track line would widen the column. -->
			<Tabs.Root bind:value={tab} class="min-w-0">
				<Tabs.List class="mb-3">
					<Tabs.Trigger value="duplicates">{t('tools.tab_duplicates')}</Tabs.Trigger>
					<Tabs.Trigger value="split">{t('tools.tab_split')}</Tabs.Trigger>
					<Tabs.Trigger value="merge">{t('tools.tab_merge')}</Tabs.Trigger>
				<Tabs.Trigger value="extract">{t('tools.tab_extract')}</Tabs.Trigger>
				<Tabs.Trigger value="reorder">{t('tools.tab_reorder')}</Tabs.Trigger>
				</Tabs.List>
				{#if engineTab}
					<div class="-mt-1 mb-3">
						<EngineChoice bind:choice={engine} ops={priced} perTrack />
						<p class="mt-1 text-[11px] text-muted-foreground">{t('ytdata.estimate.tools_applies')}</p>
					</div>
				{/if}
				<Tabs.Content value="duplicates">
					<!-- Keyed: reopening on another playlist starts the tool over. -->
					{#key playlistId}
						<DedupTab {playlistId} {title} {editable} ondone={() => (open = false)} />
					{/key}
				</Tabs.Content>
				<Tabs.Content value="split">
					{#key playlistId}
						<SplitTab {playlistId} {title} ondone={() => (open = false)} />
					{/key}
				</Tabs.Content>
				<Tabs.Content value="merge">
					{#key playlistId}
						<MergeTab {playlistId} {title} {editable} ondone={() => (open = false)} />
					{/key}
				</Tabs.Content>
				<!-- Every tab's content is mounted at once; these two read the whole playlist as they
				     mount, so each waits until its tab is the one showing. -->
				<Tabs.Content value="extract">
					{#if tab === 'extract'}
						{#key playlistId}
							<ExtractTab {playlistId} {title} {editable} ondone={() => (open = false)} />
						{/key}
					{/if}
				</Tabs.Content>
				<Tabs.Content value="reorder">
					{#if tab === 'reorder'}
						{#key playlistId}
							<ReorderTab {playlistId} {title} {editable} ondone={() => (open = false)} />
						{/key}
					{/if}
				</Tabs.Content>
			</Tabs.Root>
		{/if}
	</Dialog.Content>
</Dialog.Root>
