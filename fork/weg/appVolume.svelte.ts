/**
 * [fork] Per-app volume control on dock items.
 *
 * - Scroll over an item to change the volume of that app.
 * - Ctrl+click an item to mute / unmute it.
 * - While doing so the item tooltip shows the app volume, until the pointer leaves.
 *
 * Hooked into upstream via a single `{@attach}` in `UserApplicationItem.svelte` (see FORK.md).
 */
import { invoke, SeelenCommand, SeelenEvent, subscribe } from "@seelen-ui/lib";
import type { MediaDevice, MediaDeviceSession, UserAppWindow } from "@seelen-ui/lib/types";
import type { Attachment } from "svelte/attachments";
import type { AppOrFileWegItem } from "src/ui/svelte/weg/types.ts";
import "./wheelBridge.ts";

const VOLUME_STEP = 0.05;
// a mouse wheel notch is 100, touchpads send many small deltas that are accumulated up to half a notch
const WHEEL_NOTCH_DELTA = 100;
const WHEEL_STEP_DELTA = 50;
const WHEEL_LINES_PER_NOTCH = 3;
// how long a requested change is trusted over the system value, until the system reports it back
const PENDING_TTL_MS = 1000;

const STRINGS = {
  level: (percent: number) => `Volume: ${percent}%`,
  muted: "Muted",
  noSession: "No audio session",
};

// null until first requested, so the dock doesn't wait for (or receive) audio data it never uses
let outputs = $state.raw<MediaDevice[] | null>(null);
let outputsRequested = false;

/** Starts tracking the audio outputs, called when the pointer first enters a dock item */
function requestOutputs() {
  if (outputsRequested) return;
  outputsRequested = true;
  subscribe(SeelenEvent.MediaOutputs, ({ payload }) => {
    outputs = payload;
  }).catch(console.error);
  invoke(SeelenCommand.GetMediaDevices)
    .then(([, devices]) => {
      // an event may have already brought newer data
      outputs ??= devices;
    })
    .catch(console.error);
}

/** Latest-wins volume requests per session: one in flight, the newest level is sent when it finishes */
const volumeQueue = new Map<string, { next: number | null }>();

interface AppAudioSession {
  deviceId: string;
  session: MediaDeviceSession;
}

/**
 * Attaches the volume controls to a dock item element.
 * Getters are used so the attachment is created once and always reads the latest item state.
 */
export function appVolumeControls(
  getItem: () => AppOrFileWegItem,
  getWindows: () => UserAppWindow[],
): Attachment<HTMLElement> {
  return (el) => {
    const sessions = $derived(getAudioSessionsForItem(getItem(), getWindows()));

    let active = $state(false);
    let pendingLevel = $state<number | null>(null);
    let pendingMuted = $state<boolean | null>(null);
    let pendingTimeout: ReturnType<typeof setTimeout> | undefined;
    let wheelDelta = 0;
    let upstreamTooltip: string | undefined;

    function keepPending() {
      clearTimeout(pendingTimeout);
      pendingTimeout = setTimeout(() => {
        pendingLevel = null;
        pendingMuted = null;
      }, PENDING_TTL_MS);
    }

    $effect(() => {
      if (!active || !outputs) return;
      const session = sessions[0]?.session;
      upstreamTooltip ??= el.dataset.tooltip;
      if (!session) {
        el.dataset.tooltip = STRINGS.noSession;
      } else if (pendingMuted ?? session.muted) {
        el.dataset.tooltip = STRINGS.muted;
      } else {
        el.dataset.tooltip = STRINGS.level(Math.round((pendingLevel ?? session.volume) * 100));
      }
    });

    function deactivate() {
      active = false;
      wheelDelta = 0;
      if (upstreamTooltip !== undefined) {
        el.dataset.tooltip = upstreamTooltip;
        upstreamTooltip = undefined;
      }
    }

    function onWheel(e: WheelEvent) {
      active = true;
      if (!sessions.length) return;

      wheelDelta += e.deltaMode === WheelEvent.DOM_DELTA_PIXEL
        ? e.deltaY
        : (e.deltaY / WHEEL_LINES_PER_NOTCH) * WHEEL_NOTCH_DELTA;
      if (Math.abs(wheelDelta) < WHEEL_STEP_DELTA) return;
      // one step per notch, the backend merges the notches that queue up while the dock is busy
      const steps = Math.max(1, Math.round(Math.abs(wheelDelta) / WHEEL_NOTCH_DELTA));
      const direction = wheelDelta < 0 ? 1 : -1;
      wheelDelta = 0;

      const current = pendingLevel ?? sessions[0]!.session.volume;
      pendingLevel = setAppVolume(sessions, current + direction * steps * VOLUME_STEP);
      keepPending();
    }

    // captured and stopped so the upstream click (focus / launch) doesn't run on ctrl+click
    function onClick(e: MouseEvent) {
      if (!e.ctrlKey) return;
      e.stopPropagation();
      e.preventDefault();
      active = true;
      if (!sessions.length) return;

      pendingMuted = toggleAppMute(sessions, pendingMuted ?? sessions[0]!.session.muted);
      keepPending();
    }

    el.addEventListener("pointerenter", requestOutputs);
    el.addEventListener("wheel", onWheel, { passive: true });
    el.addEventListener("click", onClick, { capture: true });
    el.addEventListener("pointerleave", deactivate);

    return () => {
      el.removeEventListener("pointerenter", requestOutputs);
      el.removeEventListener("wheel", onWheel);
      el.removeEventListener("click", onClick, { capture: true });
      el.removeEventListener("pointerleave", deactivate);
      clearTimeout(pendingTimeout);
      deactivate();
    };
  };
}

/**
 * Audio sessions belonging to a dock item, sessions on the default output device first.
 * Matching is done by executable path (apps like browsers play audio from a child process
 * other than the one owning the window) and by the process id of the item's windows.
 */
function getAudioSessionsForItem(item: AppOrFileWegItem, windows: UserAppWindow[]): AppAudioSession[] {
  const paths = new Set(
    [item.path, item.relaunch?.command, ...windows.map((w) => w.process.path)]
      .filter((p): p is string => !!p)
      .map((p) => p.toLowerCase()),
  );
  const pids = new Set(windows.map((w) => w.process.id));

  if (!outputs) return [];
  const devices = outputs.toSorted((a, b) => Number(b.isDefaultMultimedia) - Number(a.isDefaultMultimedia));

  const result: AppAudioSession[] = [];
  for (const device of devices) {
    for (const session of device.sessions) {
      if (session.isSystem) continue;
      const sessionPath = session.iconPath?.toLowerCase();
      if (pids.has(session.processId) || (sessionPath && paths.has(sessionPath))) {
        result.push({ deviceId: device.id, session });
      }
    }
  }
  return result;
}

/** Sets all the sessions to the same level, returns the applied level */
function setAppVolume(sessions: AppAudioSession[], level: number): number {
  const clamped = Math.min(1, Math.max(0, Math.round(level * 100) / 100));
  for (const { deviceId, session } of sessions) {
    queueVolumeLevel(deviceId, session.id, clamped);
  }
  return clamped;
}

/**
 * Every applied level makes the backend broadcast the whole device list to every widget, so a fast wheel
 * only sends the newest level once the previous request finished instead of one request per notch.
 */
function queueVolumeLevel(deviceId: string, sessionId: string, level: number) {
  const key = `${deviceId}\0${sessionId}`;
  const queued = volumeQueue.get(key);
  if (queued) {
    queued.next = level;
    return;
  }

  const entry: { next: number | null } = { next: null };
  volumeQueue.set(key, entry);
  const send = (value: number) => {
    invoke(SeelenCommand.SetVolumeLevel, { deviceId, sessionId, level: value })
      .catch(console.error)
      .finally(() => {
        if (entry.next === null) {
          volumeQueue.delete(key);
          return;
        }
        const next = entry.next;
        entry.next = null;
        send(next);
      });
  };
  send(level);
}

/** Brings all the sessions to the opposite of `currentlyMuted`, returns the new muted state */
function toggleAppMute(sessions: AppAudioSession[], currentlyMuted: boolean): boolean {
  const mute = !currentlyMuted;
  for (const { deviceId, session } of sessions) {
    if (session.muted === mute) continue;
    invoke(SeelenCommand.MediaToggleMute, { deviceId, sessionId: session.id }).catch(console.error);
  }
  return mute;
}
