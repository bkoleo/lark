import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { CancelIcon } from "../components/icons";
import { syncLanguageFromSettings } from "@/i18n";
import "./MeetingPrompt.css";

interface MeetingPromptPayload {
  kind: "start" | "stop" | "stop_ask";
  app: string;
}

type Mode =
  | { view: "prompt"; payload: MeetingPromptPayload }
  | { view: "recording"; startedAt: number };

const MeetingPrompt: React.FC = () => {
  const { t } = useTranslation();
  const [mode, setMode] = useState<Mode | null>(null);
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
      return () => {
        unlistenPrompt();
        unlistenRecording();
      };
    };
    setup();
  }, []);

  // Unanswered prompts dismiss themselves (collapsing back to the
  // indicator if a recording is running — the backend decides).
  useEffect(() => {
    if (mode?.view !== "prompt") return;
    const timer = setTimeout(() => answer("dismiss"), 20000);
    return () => clearTimeout(timer);
  }, [mode]);

  // Tick the elapsed timer while recording.
  useEffect(() => {
    if (mode?.view !== "recording") return;
    const ticker = setInterval(() => setClockTick((n) => n + 1), 1000);
    return () => clearInterval(ticker);
  }, [mode]);

  const answer = (action: "record" | "stop" | "dismiss" | "expand_stop") => {
    if (action === "record") recordingStartRef.current = null;
    if (action === "stop") recordingStartRef.current = null;
    setMode(null);
    invoke("meeting_prompt_action", { action });
  };

  if (!mode) return null;

  if (mode.view === "recording") {
    const elapsed = Math.max(0, Math.floor((Date.now() - mode.startedAt) / 1000));
    const mm = String(Math.floor(elapsed / 60)).padStart(2, "0");
    const ss = String(elapsed % 60).padStart(2, "0");
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
      </button>
    );
  }

  const { payload } = mode;
  const title =
    payload.kind === "start"
      ? t("meetingPrompt.detected")
      : payload.kind === "stop"
        ? t("meetingPrompt.ended")
        : t("meetingPrompt.recording");

  return (
    <div className="meeting-card fade-in">
      <div className="meeting-card-text">
        <div className="meeting-card-title">{title}</div>
        {payload.app !== "" && (
          <div className="meeting-card-app">{payload.app}</div>
        )}
      </div>
      <button
        className="meeting-card-button"
        onClick={() => answer(payload.kind === "start" ? "record" : "stop")}
      >
        <span className="meeting-card-logo" />
        {payload.kind === "start" ? t("overlay.record") : t("overlay.stop")}
      </button>
      <button
        className="meeting-card-dismiss"
        onClick={() => answer("dismiss")}
        aria-label={t("meetingPrompt.dismiss")}
      >
        <CancelIcon />
      </button>
    </div>
  );
};

export default MeetingPrompt;
