import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface ShowNextMeetingProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

/**
 * macOS only — the menu bar title is the thing being toggled, and no other
 * platform has one. Hidden entirely elsewhere rather than shown inert.
 */
export const ShowNextMeeting: React.FC<ShowNextMeetingProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const showNextMeeting = getSetting("show_next_meeting") ?? true;

    return (
      <ToggleSwitch
        checked={showNextMeeting}
        onChange={(enabled) => updateSetting("show_next_meeting", enabled)}
        isUpdating={isUpdating("show_next_meeting")}
        label={t("settings.advanced.showNextMeeting.label")}
        description={t("settings.advanced.showNextMeeting.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
        tooltipPosition="bottom"
      />
    );
  },
);
