import React from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import type { CapitalizationMode } from "@/bindings";

interface CapitalizationProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const Capitalization: React.FC<CapitalizationProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const selected = (getSetting("capitalization") ||
      "default") as CapitalizationMode;

    const options = [
      {
        value: "default",
        label: t("settings.advanced.capitalization.options.default"),
      },
      {
        value: "no_sentence_caps",
        label: t("settings.advanced.capitalization.options.noSentenceCaps"),
      },
      {
        value: "lowercase",
        label: t("settings.advanced.capitalization.options.lowercase"),
      },
    ];

    return (
      <SettingContainer
        title={t("settings.advanced.capitalization.title")}
        description={t("settings.advanced.capitalization.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <Dropdown
          options={options}
          selectedValue={selected}
          onSelect={(value) =>
            updateSetting("capitalization", value as CapitalizationMode)
          }
          disabled={isUpdating("capitalization")}
        />
      </SettingContainer>
    );
  },
);
