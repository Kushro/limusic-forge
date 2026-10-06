// The Ctrl+K palette's own commands (places to go, your playlists, app actions), ranked locally
// against what was typed. The YouTube rows below them come back already ranked and are never
// re-scored here. Pure, so `palette.check.ts` runs it under plain Node.
import { fold } from './facets.ts';

export type PaletteCommand = {
	id: string;
	group: 'goto' | 'playlists' | 'actions';
	label: string;
	/** Extra words that find the command without being shown, e.g. "preferences" for Settings. */
	keywords?: string[];
	run: () => void;
};

/** Higher is better; 0 = not a match. */
function score(q: string, cmd: PaletteCommand): number {
	const label = fold(cmd.label);
	if (label.startsWith(q)) return 4;
	if (label.split(/[^\p{L}\p{N}]+/u).some((w) => w.startsWith(q))) return 3;
	if (label.includes(q)) return 2;
	if (cmd.keywords?.some((k) => fold(k).includes(q))) return 1;
	return 0;
}

/**
 * The commands worth showing for `query`, best first, at most `limit`. An empty query keeps the
 * given order. Otherwise: label starts with the query, then a word of the label does, then the
 * label contains it anywhere, then a keyword does; ties keep the given order, and anything that
 * matches none of those is left out. Accents and case are ignored on both sides (`fold`).
 */
export function rankCommands(query: string, cmds: PaletteCommand[], limit = 8): PaletteCommand[] {
	const q = fold(query.trim());
	if (!q) return cmds.slice(0, limit);
	return cmds
		.map((cmd, i) => ({ cmd, i, s: score(q, cmd) }))
		.filter((r) => r.s > 0)
		.sort((a, b) => b.s - a.s || a.i - b.i)
		.slice(0, limit)
		.map((r) => r.cmd);
}
