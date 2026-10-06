// node --experimental-strip-types ui/src/lib/reorder.check.ts
import { anchorsFor, lisLength, movedCount, moveBlock, nudge, seededShuffle } from './reorder.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}
const s = (x: string) => x.split('');
const j = (x: string[]) => x.join('');

ok(lisLength([]) === 0 && lisLength([3, 1, 2]) === 2 && lisLength([5, 1, 6, 2, 3, 0, 4]) === 4, 'lis');

ok(movedCount(s('abcde'), s('abcde')) === 0, 'Nothing moved, nothing pending');
ok(movedCount(s('abcde'), s('eabcd')) === 1, 'One row dragged to the top is one change');
ok(movedCount(s('abcd'), s('dcba')) === 3, 'A reversal moves all but one');

// A block keeps its own order and lands in front of the row it was dropped on.
ok(j(moveBlock(s('abcdef'), [1, 3], 5)) === 'acebdf', 'Block lands before f');
ok(j(moveBlock(s('abcdef'), [4], 0)) === 'eabcdf', 'One row to the top');
ok(j(moveBlock(s('abcdef'), [0], 6)) === 'bcdefa', 'To the end');
ok(j(moveBlock(s('abcdef'), [2, 3], 3)) === 'abcdef', 'Dropped onto itself: no change');
ok(j(moveBlock(s('abcdef'), [2, 3], 4)) === 'abcdef', 'Dropped right after itself: no change');
ok(j(moveBlock(s('abc'), [], 0)) === 'abc', 'Nothing picked');

ok(j(nudge(s('abcde'), [2, 3], -1)) === 'acdbe', 'Alt+Up moves the block one up');
ok(j(nudge(s('abcde'), [2, 3], 1)) === 'abecd', 'Alt+Down moves the block one down');
ok(j(nudge(s('abcde'), [0], -1)) === 'abcde', 'Already at the top');
ok(j(nudge(s('abcde'), [4], 1)) === 'abcde', 'Already at the bottom');

const a = anchorsFor(s('abcde'), new Set(['b', 'c', 'e']));
ok(a.get('b') === 'd' && a.get('c') === 'd' && a.get('e') === null && !a.has('a'), 'Anchors');

const shuffled = seededShuffle(s('abcdefghij'), 42);
ok(j(shuffled) === j(seededShuffle(s('abcdefghij'), 42)), 'Same seed, same shuffle');
ok(j([...shuffled].sort()) === 'abcdefghij', 'A shuffle keeps every row');
ok(j(shuffled) !== j(seededShuffle(s('abcdefghij'), 43)), 'Another seed, another shuffle');

console.log('ok');
