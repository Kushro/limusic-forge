<script lang="ts">
	// Playlist tools (`playlist_tools/`), one dialog with a tab per tool, opened from a playlist's
	// ⋯ menu. Each tab is the same three beats PlaylistForge's wizards have: set it up, review what
	// it will do (and untick what you don't want), then do it, with the toast offering an undo.
	import * as Dialog from '$lib/components/ui/dialog';
	import * as Tabs from '$lib/components/ui/tabs';
	import DedupTab from './tools/DedupTab.svelte';
	import { t } from '$lib/i18n.svelte';

	export type ToolTab = 'duplicates';

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
		<Tabs.Root bind:value={tab}>
			<Tabs.List class="mb-3">
				<Tabs.Trigger value="duplicates">{t('tools.tab_duplicates')}</Tabs.Trigger>
			</Tabs.List>
			<Tabs.Content value="duplicates">
				<!-- Keyed: reopening on another playlist starts the tool over. -->
				{#key playlistId}
					<DedupTab {playlistId} {title} {editable} ondone={() => (open = false)} />
				{/key}
			</Tabs.Content>
		</Tabs.Root>
	</Dialog.Content>
</Dialog.Root>
