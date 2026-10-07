// What is being dragged right now, for the drop targets that have to react before the drop: a
// `dragover` may read a drag's types but not its data, so the sidebar can't otherwise know how
// many rows are coming, or from which playlist (it must not offer that one as a target).
export const rowDrag = $state({
	active: false,
	count: 0,
	from: null as string | null
});

export function startRowDrag(count: number, from: string | null) {
	rowDrag.active = true;
	rowDrag.count = count;
	rowDrag.from = from;
}

export function endRowDrag() {
	rowDrag.active = false;
	rowDrag.count = 0;
	rowDrag.from = null;
}
