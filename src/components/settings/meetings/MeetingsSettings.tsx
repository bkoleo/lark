import { invoke } from "@tauri-apps/api/core";
import { FolderOpen, Search } from "lucide-react";
import React, { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";

interface MeetingNote {
  title: string;
  path: string;
  modified_ms: number;
  content: string;
}

/** First transcript line as a preview, skipping the header/meta lines. */
const previewOf = (content: string): string => {
  const line = content
    .split("\n")
    .find((l) => l.startsWith("**["));
  return line ? line.replace(/\*\*/g, "").slice(0, 120) : "";
};

export const MeetingsSettings: React.FC = () => {
  const { t } = useTranslation();
  const [notes, setNotes] = useState<MeetingNote[]>([]);
  const [query, setQuery] = useState("");

  useEffect(() => {
    invoke<MeetingNote[]>("list_meeting_notes")
      .then(setNotes)
      .catch((e) => console.error("Failed to list meeting notes:", e));
  }, []);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return notes;
    return notes.filter(
      (n) =>
        n.title.toLowerCase().includes(q) ||
        n.content.toLowerCase().includes(q),
    );
  }, [notes, query]);

  return (
    <div className="flex flex-col h-full gap-3 p-4">
      <div className="flex items-center gap-2">
        <div className="relative flex-1">
          <Search
            size={14}
            className="absolute start-3 top-1/2 -translate-y-1/2 opacity-50"
          />
          <input
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("meetings.searchPlaceholder")}
            className="w-full ps-8 pe-3 py-2 text-sm rounded-lg bg-mid-gray/10 border border-mid-gray/20 focus:outline-none focus:border-logo-primary/60"
          />
        </div>
        <button
          className="flex items-center gap-1.5 px-3 py-2 text-sm rounded-lg bg-mid-gray/10 border border-mid-gray/20 hover:bg-mid-gray/20 cursor-pointer"
          onClick={() => invoke("open_meetings_folder")}
          title={t("meetings.openFolder")}
        >
          <FolderOpen size={14} />
        </button>
      </div>

      {filtered.length === 0 && (
        <p className="text-sm opacity-60 px-1 pt-2">
          {notes.length === 0
            ? t("meetings.empty")
            : t("meetings.noResults")}
        </p>
      )}

      <div className="flex flex-col gap-1 overflow-y-auto">
        {filtered.map((note) => (
          <div
            key={note.path}
            className="flex flex-col gap-0.5 px-3 py-2 rounded-lg cursor-pointer hover:bg-mid-gray/15"
            onClick={() => invoke("open_meeting_note", { path: note.path })}
          >
            <span className="text-sm font-medium">{note.title}</span>
            <span className="text-xs opacity-55 truncate">
              {previewOf(note.content)}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
};
