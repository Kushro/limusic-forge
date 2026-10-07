// Version ordering for the updater. Pure, so `version.check.ts` can run it under plain node.

/** Whether a version is a prerelease (D1): its suffix (what follows the first `-`, build metadata
 *  after `+` ignored) starts with `rc`, `beta` or `alpha`, case-insensitive. The fork's `-forge.N`
 *  suffix marks a stable release, so `1.2.0-forge.1` is not one. Cut release candidates as
 *  `1.3.0-rc.N`: a `1.2.0-forge.2-rc.1` would be stable here and order after `1.2.0-forge.2`.
 *  Twin of `is_prerelease` in src-tauri/src/commands.rs; keep both rules identical. */
export function isPrerelease(v: string): boolean {
	const core = v.split('+')[0];
	const dash = core.indexOf('-');
	if (dash < 0) return false;
	return /^(rc|beta|alpha)/i.test(core.slice(dash + 1));
}

/** `a` is a later release than `b`. `x.y.z` compares numerically, a release outranks its own
 *  prereleases (1.1.0 > 1.1.0-rc.2), and two prereleases of one version compare by suffix
 *  (rc.10 > rc.9). Anything that doesn't parse compares as not-newer, so a weird tag can never
 *  invent an update. */
export function isNewer(a: string, b: string): boolean {
	const [ac, ...ap] = a.split('-');
	const [bc, ...bp] = b.split('-');
	const pa = ac.split('.').map(Number);
	const pb = bc.split('.').map(Number);
	for (let i = 0; i < 3; i++) {
		const [x, y] = [pa[i] ?? 0, pb[i] ?? 0];
		if (x !== y) return x > y;
	}
	const [sa, sb] = [ap.join('-'), bp.join('-')];
	if (sa === sb) return false;
	if (!sa || !sb) return !sa;
	// ponytail: numeric collation, not semver's full identifier rules; right for rc.N / beta.N
	return sa.localeCompare(sb, 'en', { numeric: true }) > 0;
}
