import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { CancelIcon } from "../components/icons";
import { syncLanguageFromSettings } from "@/i18n";
import "./MeetingPrompt.css";

interface MeetingPromptPayload {
  kind: "start" | "stop";
  app: string;
}

const MeetingPrompt: React.FC = () => {
  const { t } = useTranslation();
  const [prompt, setPrompt] = useState<MeetingPromptPayload | null>(null);

  useEffect(() => {
    const setup = async () => {
      const unlisten = await listen<MeetingPromptPayload>(
        "meeting-prompt",
        async (event) => {
          await syncLanguageFromSettings();
          setPrompt(event.payload);
        },
      );
      return () => unlisten();
    };
    setup();
  }, []);

  // Unanswered prompts dismiss themselves.
  useEffect(() => {
    if (!prompt) return;
    const timer = setTimeout(() => answer("dismiss"), 20000);
    return () => clearTimeout(timer);
  }, [prompt]);

  const answer = (action: "record" | "stop" | "dismiss") => {
    setPrompt(null);
    invoke("meeting_prompt_action", { action });
  };

  if (!prompt) return null;

  return (
    <div className="meeting-card fade-in">
      <div className="meeting-card-text">
        <div className="meeting-card-title">
          {prompt.kind === "start"
            ? t("meetingPrompt.detected")
            : t("meetingPrompt.ended")}
        </div>
        <div className="meeting-card-app">{prompt.app}</div>
      </div>
      <button
        className="meeting-card-button"
        onClick={() => answer(prompt.kind === "start" ? "record" : "stop")}
      >
        <span className="meeting-card-logo" />
        {prompt.kind === "start" ? t("overlay.record") : t("overlay.stop")}
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
