/**
 * The keyboard no longer resizes the window: edge-to-edge (forced by
 * Android 15+, hence by SDK 57) neutralises `adjustResize`; the system
 * delivers IME insets and lets the app react.
 *
 * Tracking is driven by the `react-native-keyboard-controller` SharedValue
 * (`useReanimatedKeyboardAnimation().height`, 0 closed to -height open), fed
 * frame by frame on the native side (`WindowInsetsAnimation`): the composer
 * FOLLOWS the keyboard instead of jumping afterwards. Same mechanism as duogo,
 * which ruled out `KeyboardAvoidingView` (broken automatic offset); RN core's
 * `Keyboard` events were ruled out too: one-shot and late (`keyboardDidShow`),
 * and minus the system bar (`imeInsets.bottom - barInsets.bottom` in
 * `ReactRootView`).
 *
 * `max(bottom inset, keyboard height)`: keyboard closed, the navigation bar
 * margin (content runs under it edge-to-edge); open, its full height,
 * measured from the bottom of the window, which is also the bottom of the
 * screen container, so no view measurement or header offset. Replaces
 * `SafeAreaView edges={['bottom']}` on input screens; the others keep
 * SafeAreaView.
 */

import { type ReactNode, useEffect } from 'react';
import { AppState, DeviceEventEmitter, Keyboard, Platform } from 'react-native';
import { useReanimatedKeyboardAnimation } from 'react-native-keyboard-controller';
import Animated, { useAnimatedStyle } from 'react-native-reanimated';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { useColors } from './theme.ts';

export function KeyboardAvoidingContainer({ children }: { children: ReactNode }) {
  const c = useColors();
  const insets = useSafeAreaInsets();
  const { height } = useReanimatedKeyboardAnimation();
  const avoidance = useAnimatedStyle(() => ({
    paddingBottom: Math.max(insets.bottom, -height.value),
  }));
  // Screen root by construction: `flex: 1` and the background live here, not
  // in a style triplet copied at every call site.
  return (
    <Animated.View style={[{ flex: 1, backgroundColor: c.background }, avoidance]}>
      {children}
    </Animated.View>
  );
}

/** Longer than the system's keyboard animation, so a normal hide always lands first. */
const RESYNC_DELAY_MS = 600;

/**
 * Android: keyboard-controller only learns of a hide from the IME's insets
 * animation, which never reaches a window that lost focus (a sheet or picker
 * pushed in the same tick as `Keyboard.dismiss()`, the app backgrounded), so
 * its height stays at "open". React Native's own tracking reads the inset on
 * layout and does see the hide: replay the library's `keyboardDidHide` and
 * zero its values. Mounted once, inside `KeyboardProvider`.
 */
export function KeyboardResync() {
  const { height, progress } = useReanimatedKeyboardAnimation();
  useEffect(() => {
    if (Platform.OS !== 'android') return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const check = () => {
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        if (Keyboard.isVisible() || height.value === 0) return;
        DeviceEventEmitter.emit('KeyboardController::keyboardDidHide', {
          height: 0,
          duration: 0,
          timestamp: Date.now(),
          target: -1,
          type: 'default',
          appearance: 'default',
        });
        height.value = 0;
        progress.value = 0;
      }, RESYNC_DELAY_MS);
    };
    const hide = Keyboard.addListener('keyboardDidHide', check);
    const app = AppState.addEventListener('change', (state) => {
      if (state === 'active') check();
    });
    return () => {
      if (timer !== null) clearTimeout(timer);
      hide.remove();
      app.remove();
    };
  }, [height, progress]);
  return null;
}
