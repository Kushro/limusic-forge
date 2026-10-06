// Self-check for the palette's command ranking (`palette.ts`). No test runner in `ui/`, node 22
// runs TypeScript directly:
//
//     node --experimental-strip-types ui/src/lib/palette.check.ts
//
// Prints "ok" and exits 0, or throws on the first broken invariant.
import { rankCommands, type PaletteCommand } from './palette.ts';

function ok(cond: boolean, what: string): void {
	if (!cond) throw new Error(`FAIL: ${what}`);
}

const cmd = (id: string, label: string, keywords?: string[]): PaletteCommand => ({
	id,
	group: 'goto',
	label,
	keywords,
	run: () => {}
});

const ids = (q: string, cmds: PaletteCommand[], limit?: number) =>
	rankCommands(q, cmds, limit)
		.map((c) => c.id)
		.join(',');

const cmds = [
	cmd('home', 'Home'),
	cmd('search', 'Search'),
	cmd('library', 'Library'),
	cmd('lib-songs', 'Library: Songs'),
	cmd('settings', 'Settings', ['preferences', 'options']),
	cmd('import', 'Import & migrate', ['settings']),
	cmd('theme', 'Toggle theme', ['dark', 'light']),
	cmd('cancion', 'Canción Favorita'),
	cmd('mix', 'My Supermix')
];

// --- empty query: the given order, capped -------------------------------------------------------
ok(ids('', cmds, 3) === 'home,search,library', 'empty query keeps order, capped at the limit');
ok(ids('   ', cmds, 2) === 'home,search', 'whitespace counts as empty');
ok(rankCommands('', cmds).length === 8, 'default limit is 8');
ok(rankCommands('', cmds.slice(0, 2)).length === 2, 'fewer than the limit returns them all');

// --- prefix beats word prefix beats substring beats keyword -------------------------------------
ok(ids('lib', cmds) === 'library,lib-songs', 'label prefix, stable among equals');
ok(ids('songs', cmds) === 'lib-songs', 'a later word of the label starts with the query');
ok(ids('set', cmds) === 'settings,import', 'label prefix outranks a keyword-only match');
ok(ids('upermix', cmds) === 'mix', 'substring inside a word still matches');
// "My Supermix" starts with m; "Import & migrate" has a word that does; "Home" and "Toggle theme"
// only contain it, and keep their given order between them.
ok(ids('m', cmds) === 'mix,import,home,theme', 'prefix > word prefix > substring, stable');
ok(ids('ings', cmds) === 'settings,import', 'a label substring outranks a keyword substring');
ok(ids('dark', cmds) === 'theme', 'keyword match');
ok(ids('pref', cmds) === 'settings', 'keyword substring match');

// --- accents and case on either side ------------------------------------------------------------
ok(ids('cancion', cmds) === 'cancion', 'unaccented query finds an accented label');
ok(ids('CANCIÓN', cmds) === 'cancion', 'case and accents on the query are ignored');
ok(ids('favori', cmds) === 'cancion', 'second word, accent-free');
ok(ids('HOME', cmds) === 'home', 'uppercase query');

// --- limit ------------------------------------------------------------------------------------
ok(ids('s', cmds, 2) === 'search,settings', 'limit applies after ranking');
ok(rankCommands('s', cmds, 0).length === 0, 'limit 0 returns nothing');

// --- no matches -------------------------------------------------------------------------------
ok(rankCommands('zzz', cmds).length === 0, 'nothing matches, nothing returned');
ok(rankCommands('zzz', []).length === 0, 'empty list');

// --- the input is not reordered ------------------------------------------------------------------
rankCommands('set', cmds);
ok(cmds[0].id === 'home' && cmds[4].id === 'settings', 'input array untouched');

console.log('ok');
