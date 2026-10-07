// Self-check for the translation catalogs in `locales/`. No test runner in `ui/`, node 22 runs
// TypeScript directly:
//
//     node --experimental-strip-types ui/src/lib/locales.check.ts
//
// Prints "ok" and exits 0, or throws on the first broken invariant.
//
// English is the source of truth. Spanish is kept complete: every English leaf key has a Spanish
// string, no Spanish key is left over, and both use the same `{placeholders}`. The other catalogs
// fall back to English per key (`locales/index.ts`), so they may be partial, but must not carry
// keys English no longer has. Every file in `locales/` is imported as JSON, including the ones
// `index.ts` does not offer yet, so this file type-checks without Node's typings.
import ar from './locales/ar.json' with { type: 'json' };
import de from './locales/de.json' with { type: 'json' };
import en from './locales/en.json' with { type: 'json' };
import es from './locales/es.json' with { type: 'json' };
import fr from './locales/fr.json' with { type: 'json' };
import id from './locales/id.json' with { type: 'json' };
import it from './locales/it.json' with { type: 'json' };
import ja from './locales/ja.json' with { type: 'json' };
import ko from './locales/ko.json' with { type: 'json' };
import pl from './locales/pl.json' with { type: 'json' };
import ptBR from './locales/pt_BR.json' with { type: 'json' };
import ro from './locales/ro.json' with { type: 'json' };
import ru from './locales/ru.json' with { type: 'json' };
import ta from './locales/ta.json' with { type: 'json' };
import tr from './locales/tr.json' with { type: 'json' };
import uk from './locales/uk.json' with { type: 'json' };
import zhHans from './locales/zh_Hans.json' with { type: 'json' };
import zhHant from './locales/zh_Hant.json' with { type: 'json' };

type Catalog = { [key: string]: string | Catalog };

function ok(cond: boolean, what: string): void {
	if (!cond) throw new Error(`FAIL: ${what}`);
}

function leaves(node: Catalog, prefix = '', out = new Map<string, string>()): Map<string, string> {
	for (const [k, v] of Object.entries(node)) {
		const key = prefix ? `${prefix}.${k}` : k;
		if (typeof v === 'string') out.set(key, v);
		else leaves(v, key, out);
	}
	return out;
}

const placeholders = (s: string) =>
	[...s.matchAll(/\{(\w+)\}/g)]
		.map((m) => m[1])
		.sort()
		.join(',');

const enKeys = leaves(en as Catalog);
ok(enKeys.size > 0, 'en.json has keys');

// --- es: complete, nothing extra, same placeholders ----------------------------------------------
const esKeys = leaves(es as Catalog);
const missing = [...enKeys.keys()].filter((k) => !esKeys.has(k));
ok(missing.length === 0, `es.json is missing ${missing.length} keys: ${missing.slice(0, 10).join(', ')}`);
const extra = [...esKeys.keys()].filter((k) => !enKeys.has(k));
ok(extra.length === 0, `es.json has keys en.json does not: ${extra.join(', ')}`);
const empty = [...esKeys.entries()].filter(([, v]) => v.trim() === '').map(([k]) => k);
ok(empty.length === 0, `es.json has empty strings: ${empty.join(', ')}`);
const mismatched = [...esKeys.entries()]
	.filter(([k, v]) => placeholders(v) !== placeholders(enKeys.get(k)!))
	.map(([k]) => k);
ok(mismatched.length === 0, `es.json placeholders differ from en.json: ${mismatched.join(', ')}`);

// --- other catalogs: no orphan keys --------------------------------------------------------------
const others: Record<string, unknown> = {
	ar,
	de,
	fr,
	id,
	it,
	ja,
	ko,
	pl,
	pt_BR: ptBR,
	ro,
	ru,
	ta,
	tr,
	uk,
	zh_Hans: zhHans,
	zh_Hant: zhHant
};
for (const [name, catalog] of Object.entries(others)) {
	const orphans = [...leaves(catalog as Catalog).keys()].filter((k) => !enKeys.has(k));
	ok(orphans.length === 0, `${name}.json has keys en.json does not: ${orphans.join(', ')}`);
}

console.log('ok');
