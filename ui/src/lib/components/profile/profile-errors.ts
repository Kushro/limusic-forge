import { t, type TranslationKey } from '$lib/i18n.svelte';

/** A rejection from a profile command (`code` or `code: detail`) as a translated sentence. */
export function profileErrorText(e: unknown): string {
	const raw = String(e ?? '');
	const code = raw.split(':')[0].trim();
	const key = `settings.profile.errors.${code}` as TranslationKey;
	const text = t(key);
	return text === key ? raw : text;
}
