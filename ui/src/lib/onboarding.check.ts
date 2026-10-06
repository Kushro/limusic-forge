// node --experimental-strip-types ui/src/lib/onboarding.check.ts
import { answerValue, parsePrompted, pendingNotice, shouldPrompt } from './onboarding.ts';

function ok(value: boolean, message: string) {
	if (!value) throw new Error(message);
}

// Nothing detected: never ask, whatever is stored.
ok(!shouldPrompt(undefined, []), 'Nothing to import, no prompt');
ok(!shouldPrompt('garbage', []), 'Nothing to import, even with a broken value');

// Never answered: ask.
ok(shouldPrompt(undefined, ['limusic']), 'First run asks');
ok(shouldPrompt('', ['playlistforge']), 'Empty value asks');
ok(shouldPrompt('{not json', ['limusic']), 'A broken value asks again');
ok(shouldPrompt('{"answer":"maybe","sources":["limusic"]}', ['limusic']), 'An unknown answer asks again');

// Answered for what is there: quiet, including "no" and "later".
for (const answer of ['now', 'later', 'no'] as const) {
	const v = answerValue(answer, ['limusic']);
	ok(!shouldPrompt(v, ['limusic']), `Answered ${answer}: not asked again`);
	ok(parsePrompted(v)?.answer === answer, `The answer ${answer} round-trips`);
}

// A new source shows up: ask again, even after "no".
const no = answerValue('no', ['limusic']);
ok(shouldPrompt(no, ['limusic', 'playlistforge']), 'A new source asks again');
ok(!shouldPrompt(no, []), 'A source that went away does not ask');

// Answering keeps every source seen, in order, without duplicates.
const both = answerValue('later', ['playlistforge', 'limusic'], no);
ok(JSON.stringify(parsePrompted(both)?.sources) === '["limusic","playlistforge"]', 'Sources merge');
ok(!shouldPrompt(both, ['playlistforge']), 'Covered sources stay quiet');

// What Rust writes after a migration reads the same way.
ok(!shouldPrompt('{"answer":"now","sources":["limusic"]}', ['limusic']), 'Rust value parses');

// The pending-migration notice.
ok(pendingNotice(null) === null, 'No marker, no notice');
ok(pendingNotice({ expired: false, last_status: null }) === null, 'A fresh marker is just carried out');
ok(pendingNotice({ expired: false, last_status: 'retry' }) === 'retry', 'A retry within the window asks');
ok(pendingNotice({ expired: true, last_status: 'retry' }) === 'expired', 'Past the window: expired');
ok(pendingNotice({ expired: true, last_status: null }) === 'expired', 'Expired before any launch saw it');
ok(pendingNotice({ expired: false, last_status: 'expired' }) === 'expired', 'Marked expired stays expired');

console.log('ok');
