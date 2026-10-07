<script lang="ts">
	// Settings ▸ YouTube Data API: the day's quota budget (`jobs/budget.rs`, PlaylistForge's
	// planner budget). The project's daily units are split into a safety margin never spent, a reserve
	// for the scheduled backups, and the rest for the user's jobs. Each field is saved on Enter or
	// blur through `set_setting`, which refuses values out of range; the numbers below are read back
	// from Rust (`budget_get`), so they show what is in force, defaults and the reserve's floor
	// included.
	import { onMount } from 'svelte';
	import { Input } from '$lib/components/ui/input';
	import { Switch } from '$lib/components/ui/switch';
	import * as api from '$lib/api';
	import { t, type TranslationKey } from '$lib/i18n.svelte';

	let { settings }: { settings: Record<string, string> } = $props();

	type Field = {
		key: string;
		title: TranslationKey;
		hint: TranslationKey;
		min: number;
		max: number;
		/** What `budget_get` says is in force. */
		read: (b: api.BudgetInfo) => number;
	};
	// The ranges are `set_setting`'s (`jobs::budget::*_RANGE`).
	const FIELDS: Field[] = [
		{
			key: 'budget.daily_units',
			title: 'ytdata.budget.daily_units',
			hint: 'ytdata.budget.daily_units_hint',
			min: 1,
			max: 1_000_000,
			read: (b) => b.daily_units
		},
		{
			key: 'budget.safety_margin_percent',
			title: 'ytdata.budget.margin',
			hint: 'ytdata.budget.margin_hint',
			min: 0,
			max: 50,
			read: (b) => b.safety_margin_percent
		},
		{
			key: 'budget.backup_reserve_units',
			title: 'ytdata.budget.reserve',
			hint: 'ytdata.budget.reserve_hint',
			min: 0,
			max: 1_000_000,
			read: (b) => b.backup_reserve_units
		},
		{
			key: 'budget.backup_runs_per_day',
			title: 'ytdata.budget.runs',
			hint: 'ytdata.budget.runs_hint',
			min: 0,
			max: 24,
			read: (b) => b.backup_runs_per_day
		}
	];

	let budget = $state<api.BudgetInfo | null>(null);
	let inputs = $state<Record<string, string>>({});
	let error = $state('');

	async function reload() {
		try {
			budget = await api.budgetGet();
			const b = budget;
			inputs = Object.fromEntries(FIELDS.map((f) => [f.key, String(f.read(b))]));
		} catch (e) {
			error = String(e);
		}
	}
	onMount(reload);

	async function write(key: string, value: string) {
		error = '';
		try {
			await api.setSetting(key, value);
			settings[key] = value;
		} catch (e) {
			error = String(e);
		}
		await reload();
	}

	async function saveField(f: Field) {
		const raw = (inputs[f.key] ?? '').trim();
		const n = Number(raw);
		if (!/^\d+$/.test(raw) || n < f.min || n > f.max) {
			error = t('ytdata.budget.invalid', { min: f.min, max: f.max });
			if (budget) inputs[f.key] = String(f.read(budget));
			return;
		}
		if (budget && n === f.read(budget)) return;
		await write(f.key, String(n));
	}

	const reset = $derived(
		budget ? new Date(budget.next_reset).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) : ''
	);
	const fmt = (n: number) => n.toLocaleString();
</script>

<div class="divide-y divide-border/60 overflow-hidden rounded-xl border bg-card">
	{#if budget}
		<div class="px-4 py-3.5">
			<span class="text-sm font-medium">{t('ytdata.budget.today')}</span>
			<p class="mt-1 text-xs leading-relaxed text-muted-foreground">
				{t('ytdata.budget.today_spent', { spent: fmt(budget.spent_today), daily: fmt(budget.daily_units) })}
				· {t('ytdata.budget.today_jobs', { available: fmt(budget.available_for_jobs_now) })}
				· {t('ytdata.budget.today_reset', { reset })}
			</p>
			<p class="mt-1 text-xs leading-relaxed text-muted-foreground">
				{t('ytdata.budget.partition', {
					margin: fmt(budget.safety_margin_units),
					reserve: fmt(budget.backup_reserve_remaining_today)
				})}
			</p>
		</div>
	{/if}
	{#each FIELDS as f (f.key)}
		<div class="px-4 py-3.5">
			<div class="flex items-start justify-between gap-6">
				<div class="min-w-0">
					<span class="text-sm font-medium">{t(f.title)}</span>
					<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">{t(f.hint)}</p>
				</div>
				<form
					onsubmit={(e) => {
						e.preventDefault();
						void saveField(f);
					}}
				>
					<Input
						class="w-28 text-right"
						inputmode="numeric"
						aria-label={t(f.title)}
						bind:value={inputs[f.key]}
						onblur={() => saveField(f)}
					/>
				</form>
			</div>
		</div>
	{/each}
	<div class="px-4 py-3.5">
		<div class="flex items-start justify-between gap-6">
			<div class="min-w-0">
				<span class="text-sm font-medium">{t('ytdata.budget.opportunistic')}</span>
				<p class="mt-1 max-w-prose text-xs leading-relaxed text-muted-foreground">
					{t('ytdata.budget.opportunistic_hint')}
				</p>
			</div>
			<Switch
				checked={budget?.opportunistic_mode ?? true}
				onCheckedChange={(on) => write('budget.opportunistic_mode', on ? 'true' : 'false')}
			/>
		</div>
		{#if error}
			<p class="mt-2 text-xs text-destructive" role="status">{error}</p>
		{/if}
	</div>
</div>
