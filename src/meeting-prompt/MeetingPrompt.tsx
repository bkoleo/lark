import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
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
  kind: "start" | "stop" | "stop_ask" | "saved";
  app: string;
  /** Real calendar event title, when the calendar matched. */
  title?: string | null;
  /** Pre-formatted "1:00 PM – 2:00 PM", when both bounds were known. */
  time_range?: string | null;
}

type Mode =
  | { view: "prompt"; payload: MeetingPromptPayload }
  | { view: "recording"; startedAt: number };

/** Which mic the meeting recorder has open, and whether audio is arriving.
 * `fallback` = system default opened because the pinned mic wasn't attached. */
interface MicStatus {
  mic: string | null;
  flowing: boolean;
  fallback: boolean;
}

const MeetingPrompt: React.FC = () => {
  const { t } = useTranslation();
  const [mode, setMode] = useState<Mode | null>(null);
  const [micStatus, setMicStatus] = useState<MicStatus | null>(null);
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
      const unlistenRecording = await listen("meeting-recording", () => {
        if (recordingStartRef.current == null) {
          recordingStartRef.current = Date.now();
        }
        setMode({ view: "recording", startedAt: recordingStartRef.current });
      });
      const unlistenMicStatus = await listen<MicStatus>(
        "meeting-mic-status",
        (event) => setMicStatus(event.payload),
      );
      return () => {
        unlistenPrompt();
        unlistenRecording();
        unlistenMicStatus();
      };
    };
    setup();
  }, []);

  // Unanswered prompts dismiss themselves (collapsing back to the
  // indicator if a recording is running — the backend decides).
  useEffect(() => {
    if (mode?.view !== "prompt") return;
    // The "saved" confirmation is a brief beat; the actionable prompts wait
    // longer for the user to respond.
    const ms = mode.payload.kind === "saved" ? 6000 : 20000;
    const timer = setTimeout(() => answer("dismiss"), ms);
    return () => clearTimeout(timer);
  }, [mode]);

  // Tick the elapsed timer while recording.
  useEffect(() => {
    if (mode?.view !== "recording") return;
    const ticker = setInterval(() => setClockTick((n) => n + 1), 1000);
    return () => clearInterval(ticker);
  }, [mode]);

  const answer = (action: "record" | "stop" | "dismiss" | "expand_stop") => {
    if (action === "record" || action === "stop") {
      recordingStartRef.current = null;
      setMicStatus(null);
    }
    setMode(null);
    invoke("meeting_prompt_action", { action });
  };

  if (!mode) return null;

  if (mode.view === "recording") {
    const elapsed = Math.max(
      0,
      Math.floor((Date.now() - mode.startedAt) / 1000),
    );
    const mm = String(Math.floor(elapsed / 60)).padStart(2, "0");
    const ss = String(elapsed % 60).padStart(2, "0");
    // The mic label answers "which mic is this recording?" at a glance:
    // normal = the pinned mic; amber = default fallback (pinned mic not
    // attached); red NO AUDIO = the open stream is delivering silence.
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
    return (
      <button
        className="meeting-mini fade-in"
        onClick={() => answer("expand_stop")}
        aria-label={t("meetingPrompt.recording")}
      >
        <span className="meeting-mini-dot" />
        <span className="meeting-mini-time">
          {mm}:{ss}
        </span>
        {micLabel && (
          <span className={`meeting-mini-mic${micClass}`}>{micLabel}</span>
        )}
      </button>
    );
  }

  const { payload } = mode;
  const hasCalendarTitle = !!payload.title;

  // "Recorded successfully" closure beat shown after a meeting transcript is
  // written — reassurance without stealing focus, then auto-dismisses.
  if (payload.kind === "saved") {
    return (
      <div className="meeting-card saved fade-in">
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

  return (
    <div className="meeting-card fade-in">
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
              {isStart
                ? t("meetingPrompt.recordSubtitle")
                : t("meetingPrompt.stopSubtitle")}
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
