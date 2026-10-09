// node --experimental-strip-types ui/src/lib/chips.check.ts
import { CAROUSEL_DELAY_MS, CHIP_SPEED_PX_S, chipView, loopDuration } from './chips.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}
const view = (total: number, revealed: boolean, reduced = false, initial?: number, max?: number) => {
	const v = chipView(total, revealed, reduced, initial, max);
	return `${v.shown}/${v.more}${v.carousel ? '/loop' : ''}`;
};

ok(CHIP_SPEED_PX_S === 30 && CAROUSEL_DELAY_MS === 1000, 'Speed and delay');

// --- At rest: two chips and "+N" ------------------------------------------------------------
ok(view(0, false) === '0/0' && view(1, false) === '1/0' && view(2, false) === '2/0', 'Few: all, no +N');
ok(view(3, false) === '2/1' && view(9, false) === '2/7', 'More than two: +N');

// --- Revealed, five or fewer: all of them, still ----------------------------------------------
ok(view(2, true) === '2/0', 'Nothing hidden: nothing changes');
ok(view(3, true) === '3/0' && view(5, true) === '5/0', 'Up to five: all, no +N');
ok(view(5, true, true) === '5/0', 'Up to five: reduced motion changes nothing');

// --- Revealed, more than five: the carousel ---------------------------------------------------
ok(view(6, true) === '6/1/loop' && view(12, true) === '12/7/loop', 'Past five: every chip loops');
ok(view(6, false) === '2/4', 'Past five at rest: still two and +N');

// --- Reduced motion: five and "+N", never a loop ----------------------------------------------
ok(view(6, true, true) === '5/1' && view(12, true, true) === '5/7', 'Reduced motion: five still');
ok(!chipView(40, true, true).carousel, 'Reduced motion never loops');

// --- Other windows and odd input ----------------------------------------------------------------
ok(view(4, true, false, 1, 3) === '4/1/loop' && view(4, false, false, 1, 3) === '1/3', 'Custom window');
ok(view(4, true, false, 3, 2) === '4/1/loop' && view(4, true, true, 3, 2) === '3/1', 'Window below rest: rest');
ok(view(3, true, false, 3, 2) === '3/0', 'Window below rest, nothing hidden: still');
ok(view(-1, true) === '0/0' && view(3.7, false) === '2/1', 'Negative and fractional totals');

// --- loopDuration ------------------------------------------------------------------------------
ok(loopDuration(290, 4) === 9.8, 'Width plus gap at 30 px/s');
ok(loopDuration(290, 4, 60) === 4.9, 'Custom speed');
ok(loopDuration(596, 4) === 2 * loopDuration(298, 2), 'Twice the row, twice the loop: same speed');
ok(loopDuration(0, 4) === 0 && loopDuration(-5, 4) === 0, 'Nothing to move');
ok(loopDuration(100, 4, 0) === 0 && loopDuration(NaN, 4) === 0, 'No speed, no width');
ok(loopDuration(26, -10) === 26 / 30, 'A negative gap counts as none');

console.log('ok');
