import { listen, emit } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalPosition } from "@tauri-apps/api/window";
import React, { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  TranscriptionIcon,
  CancelIcon,
  CopyIcon,
  CheckIcon,
} from "../components/icons";
import "./RecordingOverlay.css";
import { commands } from "@/bindings";
import i18n, { syncLanguageFromSettings } from "@/i18n";
import { getLanguageDirection } from "@/lib/utils/rtl";

type OverlayState = "recording" | "transcribing" | "processing" | "copyReady";

/// How long the Copy pill waits before giving up and hiding itself. The text
/// is never lost — the tray's Copy Last Transcript still has it.
const COPY_READY_TIMEOUT_MS = 60_000;
/// Time the "Copied" confirmation stays on screen before the pill hides.
const COPIED_CONFIRM_MS = 900;
type MicStatus = "connecting" | "live" | "silent";

interface MicStatusPayload {
  status: MicStatus;
  device: string | null;
}

const LONG_TRANSCRIBE_TIPS = [
  "overlay.tipLocal",
  "overlay.tipMeetings",
  "overlay.tipWords",
  "overlay.tipDrag",
];

interface TranscribeInfo {
  audioSecs: number;
  estimatedSecs: number;
}

/// Determinate-looking ring driven by the time estimate: fills towards 95%
/// and the overlay disappears when transcription actually completes.
const ProgressRing: React.FC<{ progress: number }> = ({ progress }) => {
  const radius = 8.5;
  const circumference = 2 * Math.PI * radius;
  return (
    <svg width="22" height="22" viewBox="0 0 22 22" className="progress-ring">
      <circle
        cx="11"
        cy="11"
        r={radius}
        fill="none"
        stroke="rgba(255,255,255,0.22)"
        strokeWidth="2.5"
      />
      <circle
        cx="11"
        cy="11"
        r={radius}
        fill="none"
        stroke="white"
        strokeWidth="2.5"
        strokeLinecap="round"
        strokeDasharray={circumference}
        strokeDashoffset={circumference * (1 - progress)}
        transform="rotate(-90 11 11)"
        style={{ transition: "stroke-dashoffset 250ms linear" }}
      />
    </svg>
  );
};

const RecordingOverlay: React.FC = () => {
  const { t } = useTranslation();
  const [isVisible, setIsVisible] = useState(false);
  const [state, setState] = useState<OverlayState>("recording");
  const [micStatus, setMicStatus] = useState<MicStatus>("connecting");
  const [deviceName, setDeviceName] = useState<string | null>(null);
  const [levels, setLevels] = useState<number[]>(Array(16).fill(0));
  const smoothedLevelsRef = useRef<number[]>(Array(16).fill(0));
  const [transcribeInfo, setTranscribeInfo] = useState<
    (TranscribeInfo & { startedAt: number }) | null
  >(null);
  const [longMsgIdx, setLongMsgIdx] = useState(0);
  const [, setClockTick] = useState(0);
  const [copied, setCopied] = useState(false);
  const copyTimersRef = useRef<number[]>([]);
  const direction = getLanguageDirection(i18n.language);

  const clearCopyTimers = () => {
    copyTimersRef.current.forEach((id) => window.clearTimeout(id));
    copyTimersRef.current = [];
  };

  // Raw invoke, not bindings.ts — that file only regenerates in debug builds,
  // so a typed binding for a new command never reaches a release build.
  const copyReadyAction = (action: "copy" | "dismiss") =>
    invoke("copy_ready_action", { action }).catch((e) =>
      console.error("copy_ready_action failed", e),
    );

  const handleCopyClick = async () => {
    if (copied) return;
    clearCopyTimers();
    await copyReadyAction("copy");
    setCopied(true);
    copyTimersRef.current.push(
      window.setTimeout(() => copyReadyAction("dismiss"), COPIED_CONFIRM_MS),
    );
  };

  const handleCopyDismiss = (e: React.MouseEvent) => {
    e.stopPropagation();
    clearCopyTimers();
    void copyReadyAction("dismiss");
  };

  // Drive the ring + countdown while a timed transcription is running.
  useEffect(() => {
    if (!transcribeInfo) return;
    const ticker = setInterval(() => setClockTick((n) => n + 1), 250);
    return () => clearInterval(ticker);
  }, [transcribeInfo]);

  // On long audio, alternate the label between the countdown and tips.
  useEffect(() => {
    if (!transcribeInfo || transcribeInfo.audioSecs < 60) return;
    const interval = setInterval(() => setLongMsgIdx((i) => i + 1), 5000);
    return () => clearInterval(interval);
  }, [transcribeInfo]);

  const ringProgress = () => {
    if (!transcribeInfo) return 0;
    const elapsed = (Date.now() - transcribeInfo.startedAt) / 1000;
    return Math.min(0.95, elapsed / Math.max(1, transcribeInfo.estimatedSecs));
  };

  const etaText = () => {
    if (!transcribeInfo) return t("overlay.transcribing");
    const elapsed = (Date.now() - transcribeInfo.startedAt) / 1000;
    const remaining = Math.max(0, transcribeInfo.estimatedSecs - elapsed);
    if (remaining < 3) return t("overlay.almostDone");
    // Round up to 5s steps so the number doesn't twitch
    const display = Math.ceil(remaining / 5) * 5;
    return display >= 90
      ? t("overlay.etaMin", { count: Math.round(display / 60) })
      : t("overlay.etaSecs", { count: display });
  };

  const transcribingText = () => {
    if (!transcribeInfo) return t("overlay.transcribing");
    if (transcribeInfo.audioSecs >= 60 && longMsgIdx % 2 === 1) {
      return t(
        LONG_TRANSCRIBE_TIPS[
          Math.floor(longMsgIdx / 2) % LONG_TRANSCRIBE_TIPS.length
        ],
      );
    }
    return etaText();
  };

  useEffect(() => {
    const setupEventListeners = async () => {
      // Listen for show-overlay event from Rust
      const unlistenShow = await listen("show-overlay", async (event) => {
        // Sync language from settings each time overlay is shown
        await syncLanguageFromSettings();
        const overlayState = event.payload as OverlayState;
        // A new dictation supersedes any Copy pill still on screen.
        clearCopyTimers();
        setCopied(false);
        setState(overlayState);
        if (overlayState === "recording") {
          setMicStatus("connecting");
        }
        setTranscribeInfo(null);
        setLongMsgIdx(0);
        setIsVisible(true);
      });

      // Listen for hide-overlay event from Rust
      const unlistenHide = await listen("hide-overlay", () => {
        setIsVisible(false);
      });

      // A paste was withheld because the foreground app changed mid-
      // transcription: turn the pill into a Copy button.
      const unlistenCopyReady = await listen("copy-ready", async () => {
        await syncLanguageFromSettings();
        clearCopyTimers();
        setCopied(false);
        setTranscribeInfo(null);
        setState("copyReady");
        setIsVisible(true);
        copyTimersRef.current.push(
          window.setTimeout(
            () => copyReadyAction("dismiss"),
            COPY_READY_TIMEOUT_MS,
          ),
        );
      });

      // Fired when the audio being transcribed is long enough to show a
      // progress ring and time estimate
      const unlistenTranscribingInfo = await listen<TranscribeInfo>(
        "transcribing-info",
        (event) => {
          setTranscribeInfo({ ...event.payload, startedAt: Date.now() });
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
        unlistenCopyReady();
        unlistenTranscribingInfo();
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
    if ((e.target as HTMLElement).closest(".cancel-button")) return;
    // The clickable "NO AUDIO" status deep-links to settings — don't start a
    // drag from it.
    if ((e.target as HTMLElement).closest(".mic-status-clickable")) return;
    // The Copy pill is one big button. Pointer capture retargets the eventual
    // click to this wrapper (WebKit dispatches click at the capture target),
    // so capturing here would make the copy handler unreachable — the same
    // reason the two exemptions above exist. No dragging while it's a button.
    if (state === "copyReady") return;
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
        state === "recording" ? `mic-${micStatus}` : ""
      } ${state === "copyReady" ? "copy-ready" : ""}`}
    >
      {state === "copyReady" ? (
        <div
          className="overlay-main copy-row"
          role="button"
          onClick={handleCopyClick}
        >
          <div className="overlay-left">
            {copied ? (
              <CheckIcon width={14} height={14} />
            ) : (
              <CopyIcon width={14} height={14} />
            )}
          </div>
          <div className="overlay-middle">
            <div className="copy-label">
              {copied ? t("overlay.copied") : t("overlay.copy")}
            </div>
          </div>
          <div className="overlay-right">
            <div className="cancel-button" onClick={handleCopyDismiss}>
              <CancelIcon />
            </div>
          </div>
        </div>
      ) : (
        <div className="overlay-main">
          {state === "transcribing" && transcribeInfo && (
            <div className="overlay-left">
              <ProgressRing progress={ringProgress()} />
            </div>
          )}
          {state !== "recording" &&
            !(state === "transcribing" && transcribeInfo) && (
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
                      // Tuned to match the more sensitive Rust mapping so quiet
                      // speech clearly fills the bars; silence stays a flat ~2px.
                      height: `${Math.min(20, 2 + Math.pow(v, 0.6) * 18)}px`,
                      transition:
                        "height 60ms ease-out, opacity 120ms ease-out",
                      opacity: Math.max(0.25, v * 1.9), // Minimum opacity for visibility
                    }}
                  />
                ))}
              </div>
            )}
            {state === "transcribing" && (
              <div
                className={`transcribing-text ${
                  transcribeInfo && transcribeInfo.audioSecs >= 60 ? "long" : ""
                }`}
              >
                {transcribingText()}
              </div>
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

      {state === "recording" && (
        <div
          className={`mic-status-line ${micStatus} ${
            micStatus === "silent" ? "mic-status-clickable" : ""
          }`}
          style={micStatus === "silent" ? { cursor: "pointer" } : undefined}
          onClick={
            micStatus === "silent"
              ? () => {
                  // Deep-link to the Microphone section so the user can pick the
                  // right input the moment Lark warns it hears nothing.
                  emit("navigate-to-section", { section: "microphone" });
                  commands.showMainWindowCommand();
                }
              : undefined
          }
        >
          {getMicStatusText()}
        </div>
      )}
    </div>
  );
};

export default RecordingOverlay;
