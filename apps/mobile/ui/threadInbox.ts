import { useFocusEffect } from 'expo-router';
import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import { AppState } from 'react-native';

import type { ProviderActions, ThreadTotals } from '../lib/provider.ts';
import { ThreadReadPacer, ThreadTotalsStore } from '../lib/threadInbox.ts';

const stores = new WeakMap<ProviderActions, ThreadTotalsStore>();

function totalsStore(actions: ProviderActions): ThreadTotalsStore | null {
  const read = actions.threadTotals?.bind(actions);
  const changes = actions.onThreadsChanged?.bind(actions);
  if (read === undefined || changes === undefined) return null;
  let store = stores.get(actions);
  if (store === undefined) {
    store = new ThreadTotalsStore(read, changes);
    stores.set(actions, store);
  }
  return store;
}

const nothing = () => () => {};
const none = () => null;

/** The account's unread counts, read again on each change and each connection; null until known. */
export function useThreadTotals(actions: ProviderActions, generation: number): ThreadTotals | null {
  const store = useMemo(() => totalsStore(actions), [actions]);
  const totals = useSyncExternalStore(store?.subscribe ?? nothing, store?.current ?? none);
  useEffect(() => {
    store?.schedule();
  }, [store, generation]);
  return totals;
}

/**
 * Reads an open thread when it shows, and again for each reply arriving while
 * the screen has the focus and the app the foreground. Its room stays unread:
 * only the room screen reads that.
 */
export function useThreadRead(actions: ProviderActions, rid: string | null, root: string, latest: string, lastReply: number | null): void {
  const [visible, setVisible] = useState(false);
  useFocusEffect(
    useCallback(() => {
      const update = () => setVisible(AppState.currentState === 'active');
      update();
      const listener = AppState.addEventListener('change', update);
      return () => {
        listener.remove();
        setVisible(false);
      };
    }, []),
  );
  const newestReply = useRef(lastReply);
  useEffect(() => {
    newestReply.current = lastReply;
  }, [lastReply]);
  const pacer = useMemo(
    () => (actions.markThreadRead === undefined || rid === null
      ? null
      : new ThreadReadPacer(() => actions.markThreadRead!(rid, root, newestReply.current))),
    [actions, rid, root],
  );
  useEffect(() => {
    if (visible) pacer?.note(latest);
    else pacer?.flush();
  }, [pacer, visible, latest]);
  useEffect(() => () => pacer?.flush(), [pacer]);
}
