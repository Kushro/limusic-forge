<script lang="ts">
	// How far a split or a merge has got. On an account every write waits its turn (a few seconds
	// each, to stay welcome on YouTube), so a big one takes a while and says so.
	import { building } from '$lib/build.svelte';
	import { t } from '$lib/i18n.svelte';
	import { Button } from '../ui/button';

	let { onstop }: { onstop: () => void } = $props();
	const pct = $derived(building.total ? Math.round((building.done / building.total) * 100) : 0);
</script>

<div class="space-y-3 py-2" role="status" aria-live="polite">
	<p class="text-sm">
		{building.current ? t('build.writing', { playlist: building.current }) : t('build.starting')}
	</p>
	<div class="h-2 w-full overflow-hidden rounded-full bg-muted">
		<div class="h-full rounded-full bg-primary transition-[width] duration-300" style="width: {pct}%"></div>
	</div>
	<p class="text-xs text-muted-foreground tabular-nums">
		{t('build.count', { done: building.done, total: building.total })} · {t('build.paced')}
	</p>
	<div class="flex justify-end">
		<Button variant="outline" size="sm" onclick={onstop}>{t('build.stop')}</Button>
	</div>
</div>
