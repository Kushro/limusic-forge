// node --experimental-strip-types ui/src/lib/minidyn.check.ts
import { IDLE_MS, controlsVisible, miniConfig, parseMiniConfig } from './minidyn.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}
const cfg = (raw: unknown) => {
	const c = parseMiniConfig(raw);
	return `${c.mode}/${c.progress}/${c.lyrics}`;
};

// --- parseMiniConfig ---------------------------------------------------------------------------
ok(cfg({ mode: 'dynamic', progress: false, lyrics: 'side' }) === 'dynamic/false/side', 'All valid');
ok(cfg({ mode: 'classic', progress: true, lyrics: 'overlay' }) === 'classic/true/overlay', 'Defaults spelled out');
ok(cfg(undefined) === 'classic/true/overlay' && cfg(null) === 'classic/true/overlay', 'Absent');
ok(cfg('dynamic') === 'classic/true/overlay' && cfg(42) === 'classic/true/overlay', 'Not an object');
ok(cfg(true) === 'classic/true/overlay' && cfg({}) === 'classic/true/overlay', 'Empty');
ok(cfg({ mode: 'DYNAMIC', progress: false, lyrics: 'side' }) === 'classic/false/side', 'Bad mode alone');
ok(cfg({ mode: 'dynamic', progress: 'false', lyrics: 'side' }) === 'dynamic/true/side', 'Bad progress alone');
ok(cfg({ mode: 'dynamic', progress: 0, lyrics: 'side' }) === 'dynamic/true/side', 'Numeric progress');
ok(cfg({ mode: 'dynamic', progress: false, lyrics: 'left' }) === 'dynamic/false/overlay', 'Bad lyrics alone');
ok(cfg({ mode: 'dynamic', lyrics: null }) === 'dynamic/true/overlay', 'Missing and null fields');
ok(cfg(['dynamic', false, 'side']) === 'classic/true/overlay', 'An array');
ok(parseMiniConfig(null) !== parseMiniConfig(null), 'A fresh object each time');

// --- miniConfig: reads the injected global -----------------------------------------------------
const g = globalThis as { __LIMUSIC_MINI__?: unknown };
ok(miniConfig().mode === 'classic', 'No script: classic');
g.__LIMUSIC_MINI__ = { mode: 'dynamic', progress: false, lyrics: 'side' };
ok(miniConfig().mode === 'dynamic' && !miniConfig().progress && miniConfig().lyrics === 'side', 'Injected');
g.__LIMUSIC_MINI__ = 'garbage';
ok(miniConfig().mode === 'classic', 'Garbage injected: classic');
delete g.__LIMUSIC_MINI__;

// --- controlsVisible ---------------------------------------------------------------------------
ok(IDLE_MS === 2000, 'Two seconds');
const at = (now: number, over: Partial<{ inside: boolean; dragging: boolean; focusWithin: boolean }> = {}) =>
	controlsVisible({
		lastMoveAt: 1000,
		now,
		inside: over.inside ?? true,
		dragging: over.dragging ?? false,
		focusWithin: over.focusWithin ?? false
	});

ok(at(1000) && at(1500) && at(2999), 'Inside, moved under two seconds ago: up');
ok(!at(3000) && !at(10_000), 'Inside, still for two seconds: down');
ok(!at(1000, { inside: false }) && !at(1500, { inside: false }), 'Outside: down at once');
ok(at(10_000, { dragging: true }) && at(10_000, { dragging: true, inside: false }), 'Dragging: up, anywhere');
ok(at(10_000, { focusWithin: true }) && at(10_000, { focusWithin: true, inside: false }), 'Focus: up');
ok(!at(10_000, { inside: false }), 'Drag and focus over: down again');
ok(
	controlsVisible({ lastMoveAt: 0, now: 400, inside: true, dragging: false, focusWithin: false }, 500) &&
		!controlsVisible({ lastMoveAt: 0, now: 500, inside: true, dragging: false, focusWithin: false }, 500),
	'Custom idle'
);

console.log('ok');
