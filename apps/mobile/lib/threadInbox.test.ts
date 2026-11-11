import assert from 'node:assert/strict';
import { test } from 'node:test';

import { coalesced, shouldFillThreads, THREAD_FILL_PAGES, THREAD_FILL_TARGET, ThreadReadPacer, ThreadTotalsStore } from './threadInbox.ts';

const settle = () => new Promise<void>((resolve) => setImmediate(resolve));
const wait = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

test('a burst runs once, after its last call', async () => {
  let runs = 0;
  const pace = coalesced(() => runs++, 5);
  pace.call();
  pace.call();
  pace.call();
  assert.equal(runs, 0);
  await wait(15);
  assert.equal(runs, 1);
  pace.call();
  pace.cancel();
  await wait(15);
  assert.equal(runs, 1);
});

function totalsStore() {
  const state = { reads: 0, listener: null as (() => void) | null, released: 0, gate: null as Promise<void> | null };
  const store = new ThreadTotalsStore(
    async () => {
      state.reads++;
      if (state.gate !== null) await state.gate;
      return { unreadThreads: state.reads, unreadMentions: 0 };
    },
    (listener) => {
      state.listener = listener;
      return () => {
        state.released++;
        state.listener = null;
      };
    },
    1,
  );
  return { store, state };
}

test('the screens share one read per burst of changes', async () => {
  const { store, state } = totalsStore();
  let told = 0;
  const first = store.subscribe(() => told++);
  const second = store.subscribe(() => told++);
  state.listener!();
  state.listener!();
  store.schedule();
  await wait(10);
  await settle();
  assert.equal(state.reads, 1);
  assert.equal(told, 2);
  assert.deepEqual(store.current(), { unreadThreads: 1, unreadMentions: 0 });
  first();
  assert.equal(state.released, 0);
  second();
  assert.equal(state.released, 1);
});

test('a change during a read asks for one more', async () => {
  const { store, state } = totalsStore();
  let open = () => {};
  state.gate = new Promise((resolve) => {
    open = resolve;
  });
  store.subscribe(() => {});
  store.schedule();
  await wait(5);
  assert.equal(state.reads, 1);
  store.schedule();
  store.schedule();
  await wait(5);
  assert.equal(state.reads, 1);
  state.gate = null;
  open();
  await wait(5);
  assert.equal(state.reads, 2);
});

test('a failed read keeps the count on screen', async () => {
  let fail = false;
  const store = new ThreadTotalsStore(
    async () => {
      if (fail) throw new Error('down');
      return { unreadThreads: 4, unreadMentions: 1 };
    },
    () => () => {},
    1,
  );
  store.subscribe(() => {});
  store.schedule();
  await wait(5);
  fail = true;
  store.schedule();
  await wait(5);
  assert.deepEqual(store.current(), { unreadThreads: 4, unreadMentions: 1 });
});

test('a thread is read when it opens and once per burst of replies', async () => {
  let sends = 0;
  const pacer = new ThreadReadPacer(async () => {
    sends++;
  }, 2);
  pacer.note('root');
  await wait(8);
  assert.equal(sends, 1);
  pacer.note('root');
  await wait(8);
  assert.equal(sends, 1);
  pacer.note('r1');
  pacer.note('r2');
  pacer.note('r3');
  await wait(8);
  assert.equal(sends, 2);
});

test('leaving sends what is pending and nothing else', async () => {
  let sends = 0;
  const pacer = new ThreadReadPacer(async () => {
    sends++;
  }, 1000);
  pacer.flush();
  assert.equal(sends, 0);
  pacer.note('r1');
  pacer.flush();
  assert.equal(sends, 1);
  pacer.flush();
  assert.equal(sends, 1);
});

test('a failed read goes out again at the next chance', async () => {
  const results = [false, true];
  let sends = 0;
  const pacer = new ThreadReadPacer(async () => {
    sends++;
    if (results.shift() === false) throw new Error('down');
  }, 1);
  pacer.note('r1');
  await wait(5);
  assert.equal(sends, 1);
  pacer.note('r1');
  await wait(5);
  assert.equal(sends, 2);
  pacer.note('r1');
  await wait(5);
  assert.equal(sends, 2);
});

test('a short filtered list reads on while pages and budget remain', () => {
  const list = { shown: 0, next: 'c', busy: false, budget: THREAD_FILL_PAGES };
  assert.equal(shouldFillThreads(list), true);
  assert.equal(shouldFillThreads({ ...list, shown: THREAD_FILL_TARGET - 1 }), true);
  assert.equal(shouldFillThreads({ ...list, shown: THREAD_FILL_TARGET }), false, 'the view is filled');
  assert.equal(shouldFillThreads({ ...list, next: null }), false, 'the list ended');
  assert.equal(shouldFillThreads({ ...list, busy: true }), false, 'a page is on its way or failed');
  assert.equal(shouldFillThreads({ ...list, budget: 0 }), false, 'the user decides what comes next');
});

test('walking a list that never matches stops after the budget', () => {
  let budget = THREAD_FILL_PAGES;
  let fetched = 0;
  while (shouldFillThreads({ shown: 0, next: 'c', busy: false, budget })) {
    budget--;
    fetched++;
  }
  assert.equal(fetched, 4);
});
