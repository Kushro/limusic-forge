<script lang="ts">
	// Export a playlist to a file (`playlist_tools/export.rs`): CSV that this app's Import reads back,
	// JSON with everything, or M3U8 for other players. The whole playlist, every page of it.
	import { save } from '@tauri-apps/plugin-dialog';
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import { File01Icon, CodeIcon, PlayListIcon } from '@hugeicons/core-free-icons';
	import * as Dialog from '$lib/components/ui/dialog';
	import * as api from '$lib/api';
	import { toast } from '$lib/player.svelte';
	import { t } from '$lib/i18n.svelte';

	let {
		open = $bindable(false),
		playlistId,
		title
	}: { open: boolean; playlistId: string; title: string } = $props();

	let busy = $state(false);
	const FORMATS = [
		{ id: 'csv', ext: 'csv', icon: File01Icon },
		{ id: 'json', ext: 'json', icon: CodeIcon },
		{ id: 'm3u8', ext: 'm3u8', icon: PlayListIcon }
	] as const;

	// A name a file system takes: what Windows refuses in a file name, swapped for a dash.
	const fileName = (ext: string) => `${title.replace(/[<>:"/\\|?*\u0000-\u001f]/g, '-').trim() || 'playlist'}.${ext}`;

	async function run(format: (typeof FORMATS)[number]) {
		if (busy) return;
		const path = await save({
			defaultPath: fileName(format.ext),
			filters: [{ name: format.ext.toUpperCase(), extensions: [format.ext] }]
		}).catch(() => null);
		if (!path) return;
		busy = true;
		try {
			const n = await api.exportPlaylist(playlistId, title, format.id, path);
			toast.success(n === 1 ? t('export.done_one') : t('export.done', { count: n }));
			open = false;
		} catch (e) {
			toast.error(String(e));
		} finally {
			busy = false;
		}
	}
</script>

<Dialog.Root bind:open>
	<Dialog.Content class="sm:max-w-md">
		<Dialog.Header>
			<Dialog.Title>{t('export.title')}</Dialog.Title>
			<Dialog.Description>{t('export.desc')}</Dialog.Description>
		</Dialog.Header>
		<div class="grid gap-2">
			{#each FORMATS as f (f.id)}
				<button
					class="flex items-start gap-3 rounded-lg border p-3 text-left hover:bg-accent/10 disabled:opacity-50"
					disabled={busy}
					onclick={() => run(f)}
				>
					<HugeiconsIcon icon={f.icon} class="mt-0.5 h-5 w-5 shrink-0 text-primary" />
					<span>
						<span class="block text-sm font-medium">{t(`export.${f.id}`)}</span>
						<span class="block text-xs text-muted-foreground">{t(`export.${f.id}_hint`)}</span>
					</span>
				</button>
			{/each}
		</div>
	</Dialog.Content>
</Dialog.Root>
