// node --experimental-strip-types ui/src/lib/marquee.check.ts
import {
	MARQUEE_GAP_PX,
	MARQUEE_MODES,
	type MarqueeMode,
	marqueeDuration,
	marqueePhase,
	parseMarqueeMode
} from './marquee.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}
const phase = (
	mode: MarqueeMode,
	{ overflow = true, reduced = false, hovering = false, introDone = false } = {}
) => marqueePhase({ mode, overflow, reducedMotion: reduced, hovering, introDone });

ok(MARQUEE_MODES.join() === 'auto,hover,off' && MARQUEE_GAP_PX === 40, 'Modes and gap');

// --- auto: three passes, then still; looping while hovered ----------------------------------------
ok(phase('auto') === 'intro', 'auto: a new text runs its passes');
ok(phase('auto', { introDone: true }) === 'still', 'auto: still once the passes ran out');
ok(phase('auto', { hovering: true }) === 'loop', 'auto: hover during the passes loops');
ok(phase('auto', { hovering: true, introDone: true }) === 'loop', 'auto: hover after the passes loops');

// --- hover: never on its own --------------------------------------------------------------------
ok(phase('hover') === 'still' && phase('hover', { introDone: true }) === 'still', 'hover: no passes');
ok(phase('hover', { hovering: true }) === 'loop', 'hover: loops while hovered');

// --- off: never moves -----------------------------------------------------------------------------
for (const o of [{}, { hovering: true }, { introDone: true }, { hovering: true, reduced: true }]) {
	ok(phase('off', o) === 'still', `off never moves: ${JSON.stringify(o)}`);
}

// --- A line that fits never moves, in any mode, hovered or not -----------------------------------
for (const mode of MARQUEE_MODES) {
	for (const hovering of [false, true]) {
		for (const reduced of [false, true]) {
			ok(phase(mode, { overflow: false, hovering, reduced }) === 'still', `fits: ${mode} ${hovering} ${reduced}`);
		}
	}
}

// --- Reduced motion: nothing on its own, hover still scrolls --------------------------------------
ok(phase('auto', { reduced: true }) === 'still', 'Reduced motion: no passes');
ok(phase('auto', { reduced: true, hovering: true }) === 'loop', 'Reduced motion: auto hover loops');
ok(phase('hover', { reduced: true, hovering: true }) === 'loop', 'Reduced motion: hover loops');

// --- Parsing: anything unknown is auto ------------------------------------------------------------
ok(parseMarqueeMode('auto') === 'auto' && parseMarqueeMode('hover') === 'hover', 'Known modes');
ok(parseMarqueeMode('off') === 'off', 'Known mode: off');
for (const junk of [undefined, null, '', 'Auto', 'OFF', ' hover', 'loop', 0, 1, true, {}, ['off']]) {
	ok(parseMarqueeMode(junk) === 'auto', `Junk is auto: ${JSON.stringify(junk)}`);
}

// --- Duration: the carousel's 30 px/s over the text plus the gap ----------------------------------
ok(marqueeDuration(260) === 10, '260 px + 40 px gap at 30 px/s');
ok(marqueeDuration(560) === 2 * marqueeDuration(260), 'Constant speed');
ok(marqueeDuration(0) === 0 && marqueeDuration(NaN) === 0, 'Nothing to move');

console.log('ok');
