import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, LogicalPosition } from "@tauri-apps/api/window";
import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { TranscriptionIcon, CancelIcon } from "../components/icons";
import "./RecordingOverlay.css";
import { commands } from "@/bindings";
import i18n, { syncLanguageFromSettings } from "@/i18n";
import { getLanguageDirection } from "@/lib/utils/rtl";

type OverlayState = "recording" | "transcribing" | "processing";
type MicStatus = "connecting" | "live" | "silent";

interface MicStatusPayload {
  status: MicStatus;
  device: string | null;
}

interface MeetingPromptPayload {
  kind: "start" | "stop";
  app: string;
}

const RecordingOverlay: React.FC = () => {
  const { t } = useTranslation();
  const [isVisible, setIsVisible] = useState(false);
  const [state, setState] = useState<OverlayState>("recording");
  const [micStatus, setMicStatus] = useState<MicStatus>("connecting");
  const [deviceName, setDeviceName] = useState<string | null>(null);
  const [levels, setLevels] = useState<number[]>(Array(16).fill(0));
  const smoothedLevelsRef = useRef<number[]>(Array(16).fill(0));
  const [meetingPrompt, setMeetingPrompt] =
    useState<MeetingPromptPayload | null>(null);
  const direction = getLanguageDirection(i18n.language);

  // Unanswered meeting prompts dismiss themselves; the timer is cancelled
  // when dictation takes over the overlay or a button is clicked.
  useEffect(() => {
    if (!meetingPrompt) return;
    const timer = setTimeout(() => {
      invoke("meeting_prompt_action", { action: "dismiss" });
      setMeetingPrompt(null);
    }, 20000);
    return () => clearTimeout(timer);
  }, [meetingPrompt]);

  const answerMeetingPrompt = (action: "record" | "stop" | "dismiss") => {
    setMeetingPrompt(null);
    invoke("meeting_prompt_action", { action });
  };

  useEffect(() => {
    const setupEventListeners = async () => {
      // Listen for show-overlay event from Rust
      const unlistenShow = await listen("show-overlay", async (event) => {
        // Sync language from settings each time overlay is shown
        await syncLanguageFromSettings();
        const overlayState = event.payload as OverlayState;
        setMeetingPrompt(null); // dictation takes precedence over the prompt
        setState(overlayState);
        if (overlayState === "recording") {
          setMicStatus("connecting");
        }
        setIsVisible(true);
      });

      // Listen for hide-overlay event from Rust
      const unlistenHide = await listen("hide-overlay", () => {
        setMeetingPrompt(null);
        setIsVisible(false);
      });

      // Meeting detected (or call ended while recording) — Granola-style ask
      const unlistenMeetingPrompt = await listen<MeetingPromptPayload>(
        "meeting-prompt",
        async (event) => {
          await syncLanguageFromSettings();
          setMeetingPrompt(event.payload);
          setIsVisible(true);
        },
      );

      // Mic flow status from the recording watchdog
      const unlistenMicStatus = await listen<MicStatusPayload>(
        "mic-status",
        (event) => {
          setMicStatus(event.payload.status);
          setDeviceName(event.payload.device);
        },
      );

      // Listen for mic-level updates
      const unlistenLevel = await listen<number[]>("mic-level", (event) => {
        const newLevels = event.payload as number[];

        // Apply smoothing to reduce jitter
        const smoothed = smoothedLevelsRef.current.map((prev, i) => {
          const target = newLevels[i] || 0;
          return prev * 0.7 + target * 0.3; // Smooth transition
        });

        smoothedLevelsRef.current = smoothed;
        setLevels(smoothed.slice(0, 9));
      });

      // Cleanup function
      return () => {
        unlistenShow();
        unlistenHide();
        unlistenMeetingPrompt();
        unlistenMicStatus();
        unlistenLevel();
      };
    };

    setupEventListeners();
  }, []);

  // Manual window drag: the overlay is a non-activating NSPanel, which the
  // native startDragging API ignores — so we move the window ourselves from
  // pointer events. Pointer capture keeps the drag alive even when the cursor
  // briefly outruns the window.
  const dragRef = useRef<{
    mx: number;
    my: number;
    wx: number;
    wy: number;
  } | null>(null);

  const startDrag = async (e: React.PointerEvent) => {
    if ((e.target as HTMLElement).closest(".cancel-button, .prompt-button"))
      return;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    const win = getCurrentWindow();
    const [pos, scale] = await Promise.all([
      win.outerPosition(),
      win.scaleFactor(),
    ]);
    dragRef.current = {
      mx: e.screenX,
      my: e.screenY,
      wx: pos.x / scale,
      wy: pos.y / scale,
    };
  };

  const onDragMove = (e: React.PointerEvent) => {
    const d = dragRef.current;
    if (!d) return;
    getCurrentWindow().setPosition(
      new LogicalPosition(d.wx + (e.screenX - d.mx), d.wy + (e.screenY - d.my)),
    );
  };

  const endDrag = () => {
    dragRef.current = null;
  };

  const getMicStatusText = () => {
    if (micStatus === "silent") {
      return t("overlay.noAudio");
    }
    if (micStatus === "connecting") {
      return deviceName
        ? t("overlay.connectingTo", { device: deviceName })
        : t("overlay.connecting");
    }
    return deviceName ?? "";
  };

  return (
    <div
      dir={direction}
      onPointerDown={startDrag}
      onPointerMove={onDragMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      className={`recording-overlay ${isVisible ? "fade-in" : ""} ${
        !meetingPrompt && state === "recording" ? `mic-${micStatus}` : ""
      }`}
    >
      {meetingPrompt && (
        <div className="overlay-main meeting-prompt-row">
          <div className="prompt-text">
            {meetingPrompt.kind === "start"
              ? t("overlay.meetingAsk", { app: meetingPrompt.app })
              : t("overlay.meetingEnded")}
          </div>
          <button
            className="prompt-button prompt-primary"
            onClick={() =>
              answerMeetingPrompt(
                meetingPrompt.kind === "start" ? "record" : "stop",
              )
            }
          >
            {meetingPrompt.kind === "start"
              ? t("overlay.record")
              : t("overlay.stop")}
          </button>
          <div
            className="cancel-button"
            onClick={() => answerMeetingPrompt("dismiss")}
          >
            <CancelIcon />
          </div>
        </div>
      )}

      {!meetingPrompt && (
      <div className="overlay-main">
        {state !== "recording" && (
          <div className="overlay-left">
            <TranscriptionIcon />
          </div>
        )}

        <div className="overlay-middle">
          {state === "recording" && (
            <div className="bars-container">
              {levels.map((v, i) => (
                <div
                  key={i}
                  className="bar"
                  style={{
                    height: `${Math.min(20, 4 + Math.pow(v, 0.7) * 16)}px`, // Cap at 20px max height
                    transition:
                      "height 60ms ease-out, opacity 120ms ease-out",
                    opacity: Math.max(0.2, v * 1.7), // Minimum opacity for visibility
                  }}
                />
              ))}
            </div>
          )}
          {state === "transcribing" && (
            <div className="transcribing-text">{t("overlay.transcribing")}</div>
          )}
          {state === "processing" && (
            <div className="transcribing-text">{t("overlay.processing")}</div>
          )}
        </div>

        <div className="overlay-right">
          {state === "recording" && (
            <div
              className="cancel-button"
              onClick={() => {
                commands.cancelOperation();
              }}
            >
              <CancelIcon />
            </div>
          )}
        </div>
      </div>
      )}

      {!meetingPrompt && state === "recording" && (
        <div className={`mic-status-line ${micStatus}`}>
          {getMicStatusText()}
        </div>
      )}
    </div>
  );
};

export default RecordingOverlay;
