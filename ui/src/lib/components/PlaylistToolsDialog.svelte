<script lang="ts">
	// Playlist tools (`playlist_tools/`), one dialog with a tab per tool, opened from a playlist's
	// ⋯ menu. Each tab is the same three beats PlaylistForge's wizards have: set it up, review what
	// it will do (and untick what you don't want), then do it, with the toast offering an undo.
	import * as Dialog from '$lib/components/ui/dialog';
	import * as Tabs from '$lib/components/ui/tabs';
	import DedupTab from './tools/DedupTab.svelte';
	import SplitTab from './tools/SplitTab.svelte';
	import MergeTab from './tools/MergeTab.svelte';
	import { t } from '$lib/i18n.svelte';

	export type ToolTab = 'duplicates' | 'split' | 'merge';

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
			<Tabs.Root bind:value={tab}>
				<Tabs.List class="mb-3">
					<Tabs.Trigger value="duplicates">{t('tools.tab_duplicates')}</Tabs.Trigger>
					<Tabs.Trigger value="split">{t('tools.tab_split')}</Tabs.Trigger>
					<Tabs.Trigger value="merge">{t('tools.tab_merge')}</Tabs.Trigger>
				</Tabs.List>
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
			</Tabs.Root>
		{/if}
	</Dialog.Content>
</Dialog.Root>
