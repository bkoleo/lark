import React from "react";
import ReactDOM from "react-dom/client";
import MeetingPrompt from "./MeetingPrompt";
import "@/i18n";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <MeetingPrompt />
  </React.StrictMode>,
);
