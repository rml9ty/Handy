import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { Dropdown } from "../ui/Dropdown";
import { Slider } from "../ui/Slider";
import { SettingContainer } from "../ui/SettingContainer";
import { ShortcutInput } from "./ShortcutInput";
import { useSettings } from "../../hooks/useSettings";
import type { OpenInputFieldKey } from "@/bindings";

interface OpenInputFieldProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const OpenInputField: React.FC<OpenInputFieldProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, resetSetting, isUpdating } =
      useSettings();

    const enabled = getSetting("open_input_field") ?? false;
    const delayMs = getSetting("open_input_field_delay_ms") ?? 0;
    const selectedKey = (getSetting("open_input_field_key") ||
      "enter") as OpenInputFieldKey;

    const keyOptions = [
      {
        value: "enter",
        label: t("settings.advanced.openInputField.keys.enter"),
      },
      { value: "t", label: "T" },
      { value: "y", label: "Y" },
      { value: "slash", label: "/" },
    ];

    return (
      <>
        <ToggleSwitch
          checked={enabled}
          onChange={(value) => updateSetting("open_input_field", value)}
          isUpdating={isUpdating("open_input_field")}
          label={t("settings.advanced.openInputField.title")}
          description={t("settings.advanced.openInputField.description")}
          descriptionMode={descriptionMode}
          grouped={grouped}
        />
        {enabled && (
          <>
            <SettingContainer
              title={t("settings.advanced.openInputField.keyTitle")}
              description={t("settings.advanced.openInputField.keyDescription")}
              descriptionMode={descriptionMode}
              grouped={grouped}
            >
              <Dropdown
                options={keyOptions}
                selectedValue={selectedKey}
                onSelect={(value) =>
                  updateSetting(
                    "open_input_field_key",
                    value as OpenInputFieldKey,
                  )
                }
                disabled={isUpdating("open_input_field_key")}
              />
            </SettingContainer>
            <Slider
              value={delayMs}
              onChange={(value) =>
                updateSetting("open_input_field_delay_ms", value)
              }
              onReset={() => resetSetting("open_input_field_delay_ms")}
              isResetting={isUpdating("open_input_field_delay_ms")}
              min={0}
              max={300}
              step={10}
              label={t("settings.advanced.openInputField.delayTitle")}
              description={t(
                "settings.advanced.openInputField.delayDescription",
              )}
              descriptionMode={descriptionMode}
              grouped={grouped}
              formatValue={(v) => `${v}ms`}
            />
            <ToggleSwitch
              checked={getSetting("open_input_field_clear") ?? false}
              onChange={(value) =>
                updateSetting("open_input_field_clear", value)
              }
              isUpdating={isUpdating("open_input_field_clear")}
              label={t("settings.advanced.openInputField.clearTitle")}
              description={t(
                "settings.advanced.openInputField.clearDescription",
              )}
              descriptionMode={descriptionMode}
              grouped={grouped}
            />
            <ShortcutInput
              shortcutId="transcribe_slash_command"
              descriptionMode={descriptionMode}
              grouped={grouped}
            />
            <ToggleSwitch
              checked={getSetting("slash_command_digits") ?? true}
              onChange={(value) => updateSetting("slash_command_digits", value)}
              isUpdating={isUpdating("slash_command_digits")}
              label={t("settings.advanced.openInputField.digitsTitle")}
              description={t(
                "settings.advanced.openInputField.digitsDescription",
              )}
              descriptionMode={descriptionMode}
              grouped={grouped}
            />
          </>
        )}
      </>
    );
  },
);
