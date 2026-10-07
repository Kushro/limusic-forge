<script lang="ts">
	// Settings ▸ YouTube Data API: the daily check (src-tauri/src/wintask.rs, headless.rs). A Windows
	// scheduled task, "LiMusic Forge Monitor", starts `limusic-forge --monitor --all` at the chosen
	// time; with the app open the check runs in it instead. Registering again moves the task to this
	// copy (a portable folder that moved) or to a new time. Whether that run also works through the
	// job queue is `jobs.advance_jobs_headless` (on unless set to false).
	import { onMount } from 'svelte';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Switch } from '$lib/components/ui/switch';
	import * as api from '$lib/api';
	import { t } from '$lib/i18n.svelte';

	let { settings }: { settings: Record<string, string> } = $props();

	let task = $state<api.WinTaskStatus | null>(null);
	let time = $state('');
	let busy = $state(false);
	let error = $state('');

	const advance = $derived(settings['jobs.advance_jobs_headless'] !== 'false');

	function show(s: api.WinTaskStatus) {
		task = s;
		time = s.schedule_time;
	}

	onMount(async () => {
		try {
			show(await api.wintaskStatus());
		} catch (e) {
			error = String(e);
		}
	});

	async function act(run: () => Promise<api.WinTaskStatus>) {
		error = '';
		busy = true;
		try {
			show(await run());
		} catch (e) {
			error = String(e) === 'bad_time' ? t('schedule.bad_time') : String(e);
		} finally {
			busy = false;
		}
	}

	function register() {
		if (!/^\d{1,2}:\d{2}$/.test(time.trim())) {
			error = t('schedule.bad_time');
			return;
		}
		void act(() => api.wintaskRegister(time.trim()));
	}

	async function setAdvance(on: boolean) {
		error = '';
		const key = 'jobs.advance_jobs_headless';
		const before: string | undefined = settings[key];
		settings[key] = on ? 'true' : 'false';
		try {
			await api.setSetting(key, settings[key]);
		} catch (e) {
			if (before === undefined) delete settings[key];
			else settings[key] = before;
			error = String(e);
		}
	}
</script>

<div class="divide-y divide-border/60 overflow-hidden rounded-xl border bg-card">
	<div class="px-4 py-3.5">
		<p class="max-w-prose text-xs leading-relaxed text-muted-foreground">{t('schedule.hint')}</p>
		{#if task && !task.supported}
			<p class="mt-2 text-xs text-muted-foreground">{t('schedule.unsupported')}</p>
		{:else if task}
			<p class="mt-2 text-sm font-medium">
				{task.registered ? t('schedule.registered') : t('schedule.not_registered')}
				{#if task.registered && task.status}
					<span class="font-normal text-muted-foreground">· {task.status}</span>
				{/if}
			</p>
			{#if task.registered && task.next_run}
				<p class="mt-0.5 text-xs text-muted-foreground">{t('schedule.next_run', { time: task.next_run })}</p>
			{/if}
			{#if task.registered && task.last_run}
				<p class="mt-0.5 text-xs text-muted-foreground">
					{t('schedule.last_run', { time: task.last_run, result: task.last_result ?? '-' })}
				</p>
			{/if}
			{#if task.exe_moved}
				<div class="mt-2 flex flex-wrap items-center gap-x-3 gap-y-2">
					<p class="min-w-0 flex-1 text-xs break-words text-amber-500" role="status">
						{t('schedule.moved', { registered: task.registered_exe ?? '', current: task.current_exe ?? '' })}
					</p>
					<!-- Same time as now, this copy's exe: `/f` replaces the task in place. -->
					<Button
						variant="outline"
						size="sm"
						class="shrink-0"
						disabled={busy}
						onclick={() => act(() => api.wintaskRegister(task!.schedule_time))}
					>
						{t('schedule.repoint')}
					</Button>
				</div>
			{/if}
			{#if task.error}
				<p class="mt-2 text-xs text-destructive" role="status">
					{t('schedule.query_failed', { error: task.error })}
				</p>
			{/if}
		{:else if !error}
			<p class="mt-2 text-xs text-muted-foreground">{t('common.loading')}</p>
		{/if}
	</div>

	{#if task?.supported}
		<div class="px-4 py-3.5">
			<div class="flex items-start justify-between gap-6">
				<div class="min-w-0">
					<span class="text-sm font-medium">{t('schedule.time')}</span>
					<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
						{t('schedule.time_hint')}
					</p>
				</div>
				<div class="flex shrink-0 items-center gap-2">
					<form
						onsubmit={(e) => {
							e.preventDefault();
							register();
						}}
					>
						<Input type="time" class="w-28" aria-label={t('schedule.time')} bind:value={time} disabled={busy} />
					</form>
					<Button size="sm" disabled={busy} onclick={register}>
						{task.registered ? t('schedule.update') : t('schedule.register')}
					</Button>
					{#if task.registered}
						<Button variant="ghost" size="sm" disabled={busy} onclick={() => act(api.wintaskUnregister)}>
							{t('schedule.unregister')}
						</Button>
					{/if}
				</div>
			</div>
		</div>
	{/if}

	<div class="px-4 py-3.5">
		<div class="flex items-start justify-between gap-6">
			<div class="min-w-0">
				<span class="text-sm font-medium">{t('schedule.advance_jobs')}</span>
				<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
					{t('schedule.advance_jobs_hint')}
				</p>
			</div>
			<Switch checked={advance} onCheckedChange={setAdvance} aria-label={t('schedule.advance_jobs')} />
		</div>
		{#if error}
			<p class="mt-2 text-xs text-destructive" role="status">{error}</p>
		{/if}
	</div>
</div>
