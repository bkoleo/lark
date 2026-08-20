import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalPosition } from "@tauri-apps/api/window";
import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  CancelIcon,
  CheckIcon,
  RecordIcon,
  StopIcon,
} from "../components/icons";
import { syncLanguageFromSettings } from "@/i18n";
import "./MeetingPrompt.css";

interface MeetingPromptPayload {
  kind: "start" | "stop" | "stop_ask" | "saved" | "upcoming";
  app: string;
  /** Real calendar event title, when the calendar matched. */
  title?: string | null;
  /** Pre-formatted "1:00 PM – 2:00 PM", when both bounds were known. */
  time_range?: string | null;
  /** Seconds of this call already buffered, so a Record pressed now would
   * reach that far back. Null when the rewind is off or not running. */
  buffered_secs?: number | null;
  /** "upcoming" only: how many minutes out the event is. */
  minutes?: number | null;
}

/** A call is being buffered and Record is one click away. Sent every time
 * the pill is shown, with the buffer depth measured by the backend. */
interface MeetingReadyPayload {
  buffered_secs: number;
  cap_secs: number;
}

type Mode =
  | { view: "prompt"; payload: MeetingPromptPayload }
  | { view: "recording"; startedAt: number }
  | { view: "ready"; buffered: number; cap: number; at: number };

/** Buffer depth in the pill, still climbing since the event arrived and
 * never past the window it is filling. */
const rewindSecs = (mode: { buffered: number; cap: number; at: number }) =>
  Math.min(mode.cap, mode.buffered + Math.floor((Date.now() - mode.at) / 1000));

const clock = (secs: number) => {
  const mm = String(Math.floor(secs / 60)).padStart(2, "0");
  const ss = String(Math.floor(secs) % 60).padStart(2, "0");
  return `${mm}:${ss}`;
};

/** Which mic the meeting recorder has open, and whether audio is arriving.
 * `fallback` = system default opened because the pinned mic wasn't attached. */
interface MicStatus {
  mic: string | null;
  flowing: boolean;
  fallback: boolean;
}

/** Attached input device, from the existing get_available_microphones
 * command (its synthetic "default" row is filtered out — the picker's own
 * Automatic row is the reset). */
interface MicOption {
  index: string;
  name: string;
  is_default: boolean;
}

/** Unanswered picker menus close themselves — the window grew to fit the
 * menu, and a stray click elsewhere on screen can't reach a nonactivating
 * panel to dismiss it. */
const PICKER_AUTO_CLOSE_MS = 12_000;

const MeetingPrompt: React.FC = () => {
  const { t } = useTranslation();
  const [mode, setMode] = useState<Mode | null>(null);
  const [micStatus, setMicStatus] = useState<MicStatus | null>(null);
  const [micOptions, setMicOptions] = useState<MicOption[] | null>(null);
  const [, setClockTick] = useState(0);
  // Survives prompt<->recording swaps so the timer doesn't reset when the
  // card expands and collapses.
  const recordingStartRef = React.useRef<number | null>(null);

  useEffect(() => {
    const setup = async () => {
      const unlistenPrompt = await listen<MeetingPromptPayload>(
        "meeting-prompt",
        async (event) => {
          await syncLanguageFromSettings();
          setMode({ view: "prompt", payload: event.payload });
        },
      );
      const unlistenRecording = await listen<{ elapsed_secs?: number }>(
        "meeting-recording",
        (event) => {
          if (recordingStartRef.current == null) {
            // Seeded from the backend so a recording promoted out of the
            // rewind buffer opens at what it already holds — 10:32, not
            // 00:00 — rather than reading as if the call started now.
            recordingStartRef.current =
              Date.now() - (event.payload?.elapsed_secs ?? 0) * 1000;
          }
          setMode({ view: "recording", startedAt: recordingStartRef.current });
        },
      );
      const unlistenReady = await listen<MeetingReadyPayload>(
        "meeting-ready",
        async (event) => {
          await syncLanguageFromSettings();
          setMode({
            view: "ready",
            buffered: event.payload.buffered_secs,
            cap: event.payload.cap_secs,
            at: Date.now(),
          });
        },
      );
      const unlistenMicStatus = await listen<MicStatus>(
        "meeting-mic-status",
        (event) => setMicStatus(event.payload),
      );
      return () => {
        unlistenPrompt();
        unlistenRecording();
        unlistenReady();
        unlistenMicStatus();
      };
    };
    setup();
  }, []);

  // Unanswered prompts put themselves away. "collapse", not "dismiss": a
  // card that timed out is not a refusal, and the backend decides what the
  // window falls back to — the recording pill, the Record-ready pill, or
  // nothing at all.
  useEffect(() => {
    if (mode?.view !== "prompt") return;
    // The "saved" confirmation is a brief beat; the actionable prompts wait
    // longer for the user to respond; the "upcoming" reminder holds until
    // the meeting it announces has started, then puts itself away — capped,
    // so a long reminder lead can't park a card on screen for an hour.
    const ms =
      mode.payload.kind === "saved"
        ? 6000
        : mode.payload.kind === "upcoming"
          ? Math.min(mode.payload.minutes ?? 1, 5) * 60000
          : 20000;
    const timer = setTimeout(() => answer("collapse"), ms);
    return () => clearTimeout(timer);
  }, [mode]);

  // Tick the elapsed timer while recording, and the rewind depth while a
  // call is being buffered.
  useEffect(() => {
    if (mode?.view !== "recording" && mode?.view !== "ready") return;
    const ticker = setInterval(() => setClockTick((n) => n + 1), 1000);
    return () => clearInterval(ticker);
  }, [mode]);

  const answer = (
    action:
      | "record"
      | "stop"
      | "dismiss"
      | "collapse"
      | "expand_stop"
      | "expand_record",
  ) => {
    if (action === "record" || action === "stop") {
      recordingStartRef.current = null;
      setMicStatus(null);
    }
    // Any view change retires the picker menu. No resize call needed: the
    // backend places the window for whatever it shows next.
    setMicOptions(null);
    setMode(null);
    invoke("meeting_prompt_action", { action });
  };

  const closePicker = () => {
    setMicOptions(null);
    invoke("meeting_picker_resize", { rows: 0 }).catch((e) =>
      console.error("meeting_picker_resize failed", e),
    );
  };

  const openPicker = async () => {
    try {
      const devices = (await invoke("get_available_microphones")) as
        | MicOption[]
        | null;
      // Drop the command's synthetic "default" row; the Automatic row below
      // is the picker's reset.
      const options = (devices ?? []).filter((d) => d.index !== "default");
      await invoke("meeting_picker_resize", {
        rows: Math.min(options.length + 1, 8),
      });
      setMicOptions(options);
    } catch (e) {
      console.error("mic picker failed to open", e);
    }
  };

  // Raw invoke, not bindings.ts — bindings only regenerate in debug builds,
  // so a typed binding for a new command never reaches a release build.
  // Empty device = clear the pick, back to automatic resolution.
  const pickMic = (device: string) => {
    invoke("meeting_set_mic", { device }).catch((e) =>
      console.error("meeting_set_mic failed", e),
    );
    closePicker();
  };

  // A menu nobody touches closes itself; clicks outside a nonactivating
  // panel never reach us, so there is no blur to listen for.
  useEffect(() => {
    if (!micOptions) return;
    const timer = setTimeout(closePicker, PICKER_AUTO_CLOSE_MS);
    return () => clearTimeout(timer);
  }, [micOptions]);

  // Manual window drag, as on the recording overlay (a non-activating
  // NSPanel ignores the native startDragging API) — but with a movement
  // threshold before the pointer is captured. The overlay captures on
  // pointer-down, and capture retargets the eventual click to the capture
  // target (the bug that killed its Copy button). Here everything is a
  // click target — the pill expands, the mic name opens the picker, the
  // cards carry buttons — so a plain click must never see capture. Only
  // once the pointer has actually travelled does the drag begin; the same
  // retargeting then swallows the stray click at drag-end, which is
  // exactly right.
  const dragRef = React.useRef<{
    el: HTMLElement;
    pointerId: number;
    mx: number;
    my: number;
    wx: number;
    wy: number;
    captured: boolean;
  } | null>(null);

  const startDrag = async (e: React.PointerEvent) => {
    const el = e.currentTarget as HTMLElement;
    const pointerId = e.pointerId;
    const mx = e.screenX;
    const my = e.screenY;
    const win = getCurrentWindow();
    const [pos, scale] = await Promise.all([
      win.outerPosition(),
      win.scaleFactor(),
    ]);
    dragRef.current = {
      el,
      pointerId,
      mx,
      my,
      wx: pos.x / scale,
      wy: pos.y / scale,
      captured: false,
    };
  };

  const onDragMove = (e: React.PointerEvent) => {
    const d = dragRef.current;
    if (!d) return;
    const dx = e.screenX - d.mx;
    const dy = e.screenY - d.my;
    if (!d.captured) {
      if (Math.abs(dx) < 5 && Math.abs(dy) < 5) return;
      d.el.setPointerCapture(d.pointerId);
      d.captured = true;
    }
    getCurrentWindow().setPosition(new LogicalPosition(d.wx + dx, d.wy + dy));
  };

  const endDrag = () => {
    dragRef.current = null;
  };

  const dragProps = {
    onPointerDown: startDrag,
    onPointerMove: onDragMove,
    onPointerUp: endDrag,
    onPointerCancel: endDrag,
  };

  if (!mode) return null;

  // The mic label answers "which mic is this?" at a glance: normal = the
  // resolved mic; amber = default fallback (pinned mic not attached); red
  // NO AUDIO = the open stream is delivering silence. It matters as much
  // before Record as after — a rewind buffer filling from the wrong device
  // is worth catching while it can still be fixed.
  const micLabel =
    micStatus &&
    (micStatus.flowing
      ? (micStatus.mic ?? t("meetingPrompt.defaultMic"))
      : t("meetingPrompt.noAudio"));
  const micClass = micStatus
    ? !micStatus.flowing
      ? " err"
      : micStatus.fallback
        ? " warn"
        : ""
    : "";

  /** Both pills — recording and Record-ready — are the same dark pill with
   * the same clickable mic name and device picker; only the dot, the label
   * and what a click expands into differ. */
  const miniPill = (opts: {
    dotClass: string;
    pillClass?: string;
    label: string;
    ariaLabel: string;
    onClick: () => void;
  }) => (
    <div className="meeting-mini-wrap" {...dragProps}>
      <button
        className={`meeting-mini fade-in${opts.pillClass ?? ""}`}
        onClick={opts.onClick}
        aria-label={opts.ariaLabel}
      >
        <span className={opts.dotClass} />
        <span className="meeting-mini-time">{opts.label}</span>
        {micLabel && (
          <span
            className={`meeting-mini-mic${micClass} clickable`}
            title={t("meetingPrompt.pickMic")}
            onClick={(e) => {
              // The pill button expands to a card; the mic name is its own
              // target — it opens the device picker instead.
              e.stopPropagation();
              if (micOptions) {
                closePicker();
              } else {
                void openPicker();
              }
            }}
          >
            {micLabel}
          </span>
        )}
      </button>
      {micOptions && (
        <div className="meeting-mic-menu fade-in">
          <button className="meeting-mic-item" onClick={() => pickMic("")}>
            <span className="meeting-mic-item-name auto">
              {t("meetingPrompt.autoMic")}
            </span>
          </button>
          {micOptions.map((d) => (
            <button
              key={d.index}
              className="meeting-mic-item"
              onClick={() => pickMic(d.name)}
            >
              <span className="meeting-mic-item-name">{d.name}</span>
              {micStatus?.mic === d.name && (
                <CheckIcon width={11} height={11} color="#34c759" />
              )}
            </button>
          ))}
        </div>
      )}
    </div>
  );

  if (mode.view === "recording") {
    const elapsed = Math.max(
      0,
      Math.floor((Date.now() - mode.startedAt) / 1000),
    );
    return miniPill({
      dotClass: "meeting-mini-dot",
      label: clock(elapsed),
      ariaLabel: t("meetingPrompt.recording"),
      onClick: () => answer("expand_stop"),
    });
  }

  // Record, parked for the length of the call. The clock is how far back it
  // reaches, not how long anything has been recording — nothing is being
  // kept yet. Below a minute there is nothing worth promising, so the pill
  // is just the word.
  if (mode.view === "ready") {
    const depth = rewindSecs(mode);
    return miniPill({
      dotClass: "meeting-mini-dot ready",
      // Carries a word the recording pill doesn't, so the mic name gets less
      // room before the pill would outgrow its window and clip at the screen
      // edge.
      pillClass: " ready",
      label:
        depth >= 60
          ? `${t("overlay.record")} ${clock(depth)}`
          : t("overlay.record"),
      ariaLabel: t("meetingPrompt.readyAria"),
      onClick: () => answer("expand_record"),
    });
  }

  const { payload } = mode;
  const hasCalendarTitle = !!payload.title;

  // Heads-up that a calendar event is about to start. No action button —
  // recording is the detection card's offer, made when a call app actually
  // opens the mic. The × collapses rather than dismisses: closing a
  // reminder is not a refusal to record, and must never drop a rewind
  // buffer the call detector is filling.
  if (payload.kind === "upcoming") {
    return (
      <div className="meeting-card fade-in" {...dragProps}>
        <div className="meeting-card-accent" />
        <div className="meeting-card-body">
          <div className="meeting-card-text">
            <div className="meeting-card-title">
              {payload.title || t("meetingPrompt.upcomingTitle")}
            </div>
            <div className="meeting-card-time">
              {t("meetingPrompt.upcomingSubtitle", {
                minutes: payload.minutes ?? 1,
              })}
            </div>
          </div>
          <button
            className="meeting-card-dismiss"
            onClick={() => answer("collapse")}
            aria-label={t("meetingPrompt.dismiss")}
          >
            <CancelIcon />
          </button>
        </div>
      </div>
    );
  }

  // "Recorded successfully" closure beat shown after a meeting transcript is
  // written — reassurance without stealing focus, then auto-dismisses.
  if (payload.kind === "saved") {
    return (
      <div className="meeting-card saved fade-in" {...dragProps}>
        <div className="meeting-card-accent green" />
        <div className="meeting-card-body">
          <div className="meeting-card-check">
            <CheckIcon width={12} height={12} color="#248a3d" />
          </div>
          <div className="meeting-card-text">
            <div className="meeting-card-title">
              {payload.title || t("meetingPrompt.savedTitle")}
            </div>
            <div className="meeting-card-time">
              {t("meetingPrompt.savedSubtitle")}
            </div>
          </div>
          <button
            className="meeting-card-dismiss"
            onClick={() => answer("dismiss")}
            aria-label={t("meetingPrompt.dismiss")}
          >
            <CancelIcon />
          </button>
        </div>
      </div>
    );
  }

  // "start" offers to record; "stop"/"stop_ask" offer to stop (call ended,
  // or the user expanded the recording pill to stop manually).
  const isStart = payload.kind === "start";

  const title =
    payload.title ||
    (isStart
      ? t("meetingPrompt.detected")
      : payload.kind === "stop"
        ? t("meetingPrompt.ended")
        : t("meetingPrompt.recording"));

  const subtitle = isStart
    ? payload.time_range || payload.app
    : payload.kind === "stop"
      ? hasCalendarTitle
        ? t("meetingPrompt.endedWithTitle")
        : t("meetingPrompt.stopPrompt")
      : t("meetingPrompt.stopPrompt");

  // The second line of the Record button is where the rewind is promised —
  // on the control that will honour it, rather than in a note beside it.
  // Under a minute there is nothing to promise and it stays "this meeting".
  const buffered = payload.buffered_secs ?? 0;
  const recordSubtitle =
    isStart && buffered >= 60
      ? t("meetingPrompt.recordRewind", { time: clock(buffered) })
      : t("meetingPrompt.recordSubtitle");

  return (
    <div className="meeting-card fade-in" {...dragProps}>
      <div className={`meeting-card-accent${isStart ? "" : " blue"}`} />
      <div className="meeting-card-body">
        <div className="meeting-card-text">
          <div className="meeting-card-title">{title}</div>
          <div className="meeting-card-time">{subtitle}</div>
        </div>
        <button
          className={`meeting-card-button${isStart ? "" : " blue"}`}
          onClick={() => answer(isStart ? "record" : "stop")}
        >
          {isStart ? (
            <RecordIcon width={16} height={16} color="#1a1200" />
          ) : (
            <StopIcon width={16} height={16} color="#ffffff" />
          )}
          <span className="meeting-card-button-label">
            <span className="l1">
              {isStart ? t("overlay.record") : t("overlay.stop")}
            </span>
            <span className="l2">
              {isStart ? recordSubtitle : t("meetingPrompt.stopSubtitle")}
            </span>
          </span>
        </button>
        <button
          className="meeting-card-dismiss"
          onClick={() => answer("dismiss")}
          aria-label={t("meetingPrompt.dismiss")}
        >
          <CancelIcon />
        </button>
      </div>
    </div>
  );
};

export default MeetingPrompt;
