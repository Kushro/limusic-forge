// The first-run import prompt (D8): "We found data from LiMusic / PlaylistForge. Import it?"
// Asked once; asked again only when a source shows up that the last answer did not cover. The
// answer lives in the `onboarding_import_prompted` setting as JSON. Pure, so the check can run
// under plain Node (onboarding.check.ts).

/** A place there is something to import from. */
export type ImportSourceId = 'limusic' | 'playlistforge';

export type PromptAnswer = 'now' | 'later' | 'no';

export interface PromptedValue {
	answer: PromptAnswer;
	/** The sources that were on screen when the user answered. */
	sources: string[];
}

const ANSWERS: readonly string[] = ['now', 'later', 'no'];

/** The stored value, or null when it is missing or not ours. */
export function parsePrompted(value: string | null | undefined): PromptedValue | null {
	if (!value) return null;
	try {
		const v = JSON.parse(value);
		if (!v || typeof v !== 'object' || !ANSWERS.includes(v.answer) || !Array.isArray(v.sources)) {
			return null;
		}
		return { answer: v.answer, sources: v.sources.filter((s: unknown) => typeof s === 'string') };
	} catch {
		return null;
	}
}

/** Whether to show the prompt: something is detected, and either nothing was ever answered or a
 *  detected source is not among those the last answer covered. "No" included: a new source is a
 *  new question. */
export function shouldPrompt(value: string | null | undefined, detected: string[]): boolean {
	if (detected.length === 0) return false;
	const prev = parsePrompted(value);
	if (!prev) return true;
	return detected.some((s) => !prev.sources.includes(s));
}

/** The parts of the pending migration marker the notice depends on (api.MigratePending). */
export interface PendingMarker {
	expired: boolean;
	last_status: string | null;
}

/** Which "pending migration: retry now / cancel" notice a marker calls for: `expired` when it is
 *  past its ten-minute window (never carried out on its own any more), `retry` when the last launch
 *  could not do it (LiMusic open, a file held), null when there is nothing to ask (no marker, or
 *  one the next launch will just carry out). */
export function pendingNotice(p: PendingMarker | null | undefined): 'expired' | 'retry' | null {
	if (!p) return null;
	if (p.expired || p.last_status === 'expired') return 'expired';
	if (p.last_status === 'retry') return 'retry';
	return null;
}

/** The value to store for an answer: what was answered, and every source seen so far. */
export function answerValue(
	answer: PromptAnswer,
	detected: string[],
	value?: string | null
): string {
	const prev = parsePrompted(value)?.sources ?? [];
	const sources = [...prev];
	for (const s of detected) if (!sources.includes(s)) sources.push(s);
	const out: PromptedValue = { answer, sources };
	return JSON.stringify(out);
}
