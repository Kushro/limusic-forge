<script lang="ts">
	// Settings ▸ YouTube Data API: how playlist changes are made (`jobs/engine.rs`). The engine
	// (`playlist_engine`: auto, the Data API, or InnerTube, the way YouTube Music's own site writes),
	// whether InnerTube changes go through the job queue too (`job_queue_mode`), and the priority a
	// new job gets (`jobs.default_job_priority`). Rust validates each value; a refused one is put
	// back and its message shown.
	import * as RadioGroup from '$lib/components/ui/radio-group';
	import * as Select from '$lib/components/ui/select';
	import * as api from '$lib/api';
	import { t } from '$lib/i18n.svelte';
	import { parseEngine, ytdata, type PlaylistEngine } from '$lib/ytdata.svelte';

	let { settings }: { settings: Record<string, string> } = $props();

	const ENGINES: PlaylistEngine[] = ['auto', 'ytdata', 'innertube'];
	const QUEUE_MODES = ['unified', 'ytdata_only'] as const;
	type QueueMode = (typeof QUEUE_MODES)[number];
	const PRIORITIES = ['1', '2', '3'] as const;
	type Priority = (typeof PRIORITIES)[number];

	const engine = $derived(parseEngine(settings.playlist_engine));
	const queueMode = $derived<QueueMode>(settings.job_queue_mode === 'ytdata_only' ? 'ytdata_only' : 'unified');
	const priority = $derived<Priority>(
		PRIORITIES.find((p) => p === settings['jobs.default_job_priority']?.trim()) ?? '2'
	);

	let error = $state('');

	async function save(key: string, value: string) {
		error = '';
		const before: string | undefined = settings[key];
		settings[key] = value;
		try {
			await api.setSetting(key, value);
			if (key === 'playlist_engine') ytdata.engine = parseEngine(value);
		} catch (e) {
			if (before === undefined) delete settings[key];
			else settings[key] = before;
			error = String(e);
		}
	}
</script>

<div class="divide-y divide-border/60 overflow-hidden rounded-xl border bg-card">
	<div class="px-4 py-3.5">
		<span class="text-sm font-medium">{t('ytdata.engine.label')}</span>
		<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">{t('ytdata.engine.hint')}</p>
		<RadioGroup.Root
			value={engine}
			onValueChange={(v) => save('playlist_engine', v)}
			class="mt-2 gap-0"
			aria-label={t('ytdata.engine.label')}
		>
			{#each ENGINES as key (key)}
				<label class="flex cursor-pointer items-start gap-2 rounded-md px-1 py-1.5 text-sm hover:bg-accent/10">
					<RadioGroup.Item value={key} class="mt-0.5" />
					<span>
						{t(`ytdata.engine.${key}`)}
						<span class="block text-xs text-muted-foreground">{t(`ytdata.engine.${key}_hint`)}</span>
					</span>
				</label>
			{/each}
		</RadioGroup.Root>
	</div>

	<div class="px-4 py-3.5">
		<div class="flex items-start justify-between gap-6">
			<div class="min-w-0">
				<span class="text-sm font-medium">{t('ytdata.engine.queue_label')}</span>
				<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
					{t(`ytdata.engine.queue_${queueMode}_hint`)}
				</p>
			</div>
			<Select.Root type="single" value={queueMode} onValueChange={(v) => save('job_queue_mode', v)}>
				<Select.Trigger class="w-48 shrink-0" aria-label={t('ytdata.engine.queue_label')}>
					<span class="flex-1 text-left">{t(`ytdata.engine.queue_${queueMode}`)}</span>
				</Select.Trigger>
				<Select.Content>
					{#each QUEUE_MODES as mode (mode)}
						<Select.Item value={mode} label={t(`ytdata.engine.queue_${mode}`)}>
							{t(`ytdata.engine.queue_${mode}`)}
						</Select.Item>
					{/each}
				</Select.Content>
			</Select.Root>
		</div>
	</div>

	<div class="px-4 py-3.5">
		<div class="flex items-start justify-between gap-6">
			<div class="min-w-0">
				<span class="text-sm font-medium">{t('ytdata.engine.priority_label')}</span>
				<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
					{t('ytdata.engine.priority_hint')}
				</p>
			</div>
			<Select.Root
				type="single"
				value={priority}
				onValueChange={(v) => save('jobs.default_job_priority', v)}
			>
				<Select.Trigger class="w-48 shrink-0" aria-label={t('ytdata.engine.priority_label')}>
					<span class="flex-1 text-left">{t(`ytdata.engine.priority_${priority}`)}</span>
				</Select.Trigger>
				<Select.Content>
					{#each PRIORITIES as p (p)}
						<Select.Item value={p} label={t(`ytdata.engine.priority_${p}`)}>
							{t(`ytdata.engine.priority_${p}`)}
						</Select.Item>
					{/each}
				</Select.Content>
			</Select.Root>
		</div>
		{#if error}
			<p class="mt-2 text-xs text-destructive" role="status">{error}</p>
		{/if}
	</div>
</div>
