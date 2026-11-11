import assert from 'node:assert/strict';
import { test } from 'node:test';

import { inThreadScope, threadMentioned, threadParticipated, writtenMention } from './threadScope.ts';

const nobody = { rootAuthor: 'other', repliers: ['eve'], direct: false };

test('I participate as the root author, a replier, or by the conversation being direct', () => {
  assert.equal(threadParticipated('me', nobody), false);
  assert.equal(threadParticipated('me', { ...nobody, rootAuthor: 'me' }), true);
  assert.equal(threadParticipated('me', { ...nobody, repliers: ['eve', 'me'] }), true);
  assert.equal(threadParticipated('me', { ...nobody, direct: true }), true);
});

test('a mention comes from the provider count or from a text returned', () => {
  const names = writtenMention('me');
  assert.equal(threadMentioned({ mentions: 0, texts: ['hello', null], names }), false);
  assert.equal(threadMentioned({ mentions: 2, texts: [], names }), true);
  assert.equal(threadMentioned({ mentions: 0, texts: [null, 'hi @me'], names }), true);
});

test('a written mention is the whole name', () => {
  const names = writtenMention('jo.ann');
  for (const text of ['@jo.ann', 'hi @jo.ann!', '(@JO.ANN)', 'see @jo.ann.', '@jo.ann, ok']) assert.equal(names(text), true, text);
  for (const text of ['@jo.anne', 'x@jo.ann', '@jo.ann-b', '@jo.ann.b', '@joXann', 'jo.ann']) assert.equal(names(text), false, text);
});

test('an unknown username names nobody', () => {
  assert.equal(writtenMention(null)('@me'), false);
  assert.equal(writtenMention('')('@'), false);
});

test('participated and mentioned overlap, others is neither', () => {
  const both = { participated: true, mentioned: true };
  const neither = { participated: false, mentioned: false };
  assert.equal(inThreadScope(both, 'all'), true);
  assert.equal(inThreadScope(both, 'participated'), true);
  assert.equal(inThreadScope(both, 'mentioned'), true);
  assert.equal(inThreadScope(both, 'others'), false);
  assert.equal(inThreadScope(neither, 'all'), true);
  assert.equal(inThreadScope(neither, 'participated'), false);
  assert.equal(inThreadScope(neither, 'mentioned'), false);
  assert.equal(inThreadScope(neither, 'others'), true);
  assert.equal(inThreadScope({ participated: true, mentioned: false }, 'others'), false);
  assert.equal(inThreadScope({ participated: false, mentioned: true }, 'others'), false);
});
