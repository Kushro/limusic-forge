// Rearranging a playlist by hand: the staged order on the playlist page, before it is written.
// Pure, so `reorder.check.ts` runs it under plain Node. The write itself is the backend's
// (`playlist_tools::lis`), which works out the fewest moves; this side only needs the same count to
// say how many changes are pending.

/** Length of a longest strictly increasing subsequence. O(n log n). */
export function lisLength(seq: number[]): number {
	const tails: number[] = [];
	for (const v of seq) {
		let lo = 0;
		let hi = tails.length;
		while (lo < hi) {
			const mid = (lo + hi) >> 1;
			if (tails[mid] < v) lo = mid + 1;
			else hi = mid;
		}
		tails[lo] = v;
	}
	return tails.length;
}

/** How many rows have to move to turn `before` into `after` (both lists of row handles, the same
 *  rows): every row outside the longest run already in order. 0 when nothing changed. */
export function movedCount(before: string[], after: string[]): number {
	const pos = new Map(after.map((h, i) => [h, i]));
	const seq = before.map((h) => pos.get(h)).filter((p): p is number => p !== undefined);
	return seq.length - lisLength(seq);
}

/** `items` with the rows at `picked` taken out and put back, in their own order, in front of the
 *  row that was at `to` (`items.length` for the end). Dropping a block onto itself changes nothing. */
export function moveBlock<T>(items: T[], picked: number[], to: number): T[] {
	const set = new Set(picked.filter((i) => i >= 0 && i < items.length));
	if (!set.size) return items;
	const block = items.filter((_, i) => set.has(i));
	const rest: T[] = [];
	let at = 0;
	for (let i = 0; i < items.length; i++) {
		if (i === to) at = rest.length;
		if (!set.has(i)) rest.push(items[i]);
	}
	if (to >= items.length) at = rest.length;
	return [...rest.slice(0, at), ...block, ...rest.slice(at)];
}

/** Move the rows at `picked` one step up (`-1`) or down (`+1`) as a block, for Alt+↑/↓. */
export function nudge<T>(items: T[], picked: number[], dir: -1 | 1): T[] {
	if (!picked.length) return items;
	const sorted = [...picked].sort((a, b) => a - b);
	if (dir < 0) {
		const first = sorted[0];
		return first === 0 ? items : moveBlock(items, sorted, first - 1);
	}
	const last = sorted[sorted.length - 1];
	return last >= items.length - 1 ? items : moveBlock(items, sorted, last + 2);
}

/** For each handle in `removed`, the handle of the first row after it that stays: where an undo puts
 *  it back. `null` when only removed rows follow it (it goes back last). */
export function anchorsFor(handles: string[], removed: Set<string>): Map<string, string | null> {
	const out = new Map<string, string | null>();
	let next: string | null = null;
	for (let i = handles.length - 1; i >= 0; i--) {
		const h = handles[i];
		if (removed.has(h)) out.set(h, next);
		else next = h;
	}
	return out;
}

/** A shuffle that a seed reproduces (mulberry32 + Fisher–Yates), so "Shuffle the order" can say
 *  which shuffle it was and a test can pin one. */
export function seededShuffle<T>(items: T[], seed: number): T[] {
	let s = seed >>> 0;
	const rand = () => {
		s = (s + 0x6d2b79f5) >>> 0;
		let x = s;
		x = Math.imul(x ^ (x >>> 15), x | 1);
		x ^= x + Math.imul(x ^ (x >>> 7), x | 61);
		return ((x ^ (x >>> 14)) >>> 0) / 4294967296;
	};
	const out = items.slice();
	for (let i = out.length - 1; i > 0; i--) {
		const j = Math.floor(rand() * (i + 1));
		[out[i], out[j]] = [out[j], out[i]];
	}
	return out;
}
