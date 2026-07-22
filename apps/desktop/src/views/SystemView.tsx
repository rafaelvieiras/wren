import { Info } from "lucide-react";
import { useTranslation } from "react-i18next";
import { SectionHeader } from "@/components/SectionHeader";
import { Field } from "@/components/Field";
import { SettingRow } from "@/components/SettingRow";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useCheckUpdates } from "@/lib/queries";
import { useSettingsStore } from "@/lib/store";
import type { LogLevel, PasteMethod } from "@/lib/tauri";

/**
 * Text delivery methods (mirrors the domain's `PasteMethod`). The `value` is the
 * backend contract; `labelKey`/`hintKey` point at the `system` i18n namespace.
 */
const PASTE_METHODS: {
  value: PasteMethod;
  labelKey: string;
  hintKey: string;
}[] = [
  {
    value: "paste",
    labelKey: "delivery.method.paste.label",
    hintKey: "delivery.method.paste.hint",
  },
  {
    value: "ctrl_shift_v",
    labelKey: "delivery.method.ctrlShiftV.label",
    hintKey: "delivery.method.ctrlShiftV.hint",
  },
  {
    value: "type",
    labelKey: "delivery.method.type.label",
    hintKey: "delivery.method.type.hint",
  },
  {
    value: "wtype",
    labelKey: "delivery.method.wtype.label",
    hintKey: "delivery.method.wtype.hint",
  },
];

/**
 * Logger verbosity (mirrors the domain's `LogLevel`). `value` is the backend
 * contract; `labelKey` points at the `system` i18n namespace.
 */
const LOG_LEVELS: { value: LogLevel; labelKey: string }[] = [
  { value: "error", labelKey: "logging.level.error" },
  { value: "warn", labelKey: "logging.level.warn" },
  { value: "info", labelKey: "logging.level.info" },
  { value: "debug", labelKey: "logging.level.debug" },
  { value: "trace", labelKey: "logging.level.trace" },
];

/** Subtle (i) with a tooltip hint — replaces the old tiny ⓘ marks. */
function InfoHint({ label }: { label: string }) {
  const { t } = useTranslation("system");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label={t("infoHint.ariaLabel")}
          className="inline-flex size-4 items-center justify-center rounded-sm text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <Info className="size-3.5" />
        </button>
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}

/**
 * System: text delivery, clipboard, autostart, and updates. Each concern lives
 * in its own titled Card. Autosave via the store; "Check for updates" uses the
 * `useCheckUpdates` mutation with states (idle / checking / result).
 */
export default function SystemView() {
  const { t } = useTranslation("system");
  const settings = useSettingsStore((s) => s.settings);
  const setField = useSettingsStore((s) => s.setField);
  const checkUpdates = useCheckUpdates();

  if (!settings) return null;

  // "Restore clipboard" only makes sense when pasting uses the clipboard.
  const usesClipboard =
    settings.paste_method === "paste" ||
    settings.paste_method === "ctrl_shift_v";
  const activeHintKey = PASTE_METHODS.find(
    (m) => m.value === settings.paste_method,
  )?.hintKey;

  return (
    <div className="max-w-[560px] space-y-4">
      <SectionHeader title={t("title")} description={t("description")} />

      <Card>
        <CardHeader>
          <CardTitle>{t("delivery.title")}</CardTitle>
        </CardHeader>
        <CardContent>
          <Field
            label={t("delivery.pasteMethod.label")}
            hint={activeHintKey ? t(activeHintKey) : undefined}
          >
            <Select
              value={settings.paste_method}
              onValueChange={(v) => setField("paste_method", v as PasteMethod)}
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {PASTE_METHODS.map((m) => (
                  <SelectItem key={m.value} value={m.value}>
                    {t(m.labelKey)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>

          <SettingRow
            className="mt-4 border-t border-border pt-4"
            disabled={!usesClipboard}
            label={
              <span className="inline-flex items-center gap-1.5">
                {t("delivery.restoreClipboard.label")}
                <InfoHint label={t("delivery.restoreClipboard.info")} />
              </span>
            }
            hint={
              usesClipboard
                ? t("delivery.restoreClipboard.hint.enabled")
                : t("delivery.restoreClipboard.hint.disabled")
            }
            control={
              <Switch
                checked={usesClipboard && settings.restore_clipboard}
                disabled={!usesClipboard}
                onCheckedChange={(on) => setField("restore_clipboard", on)}
              />
            }
          />
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t("startup.title")}</CardTitle>
        </CardHeader>
        <CardContent>
          <SettingRow
            className="py-0"
            label={t("startup.launchAtLogin.label")}
            hint={t("startup.launchAtLogin.hint")}
            control={
              <Switch
                checked={settings.launch_at_login}
                onCheckedChange={(on) => setField("launch_at_login", on)}
              />
            }
          />
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t("logging.title")}</CardTitle>
        </CardHeader>
        <CardContent>
          <Field label={t("logging.level.label")} hint={t("logging.level.hint")}>
            <Select
              value={settings.log_level}
              onValueChange={(v) => setField("log_level", v as LogLevel)}
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {LOG_LEVELS.map((lv) => (
                  <SelectItem key={lv.value} value={lv.value}>
                    {t(lv.labelKey)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t("updates.title")}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex items-center gap-3">
            <Button
              type="button"
              variant="secondary"
              onClick={() => checkUpdates.mutate()}
              disabled={checkUpdates.isPending}
            >
              {checkUpdates.isPending ? t("updates.checking") : t("updates.check")}
            </Button>
            {checkUpdates.isSuccess && (
              <span
                role="status"
                className="animate-fade-in text-sm text-success"
              >
                {checkUpdates.data}
              </span>
            )}
            {checkUpdates.isError && (
              <span
                role="status"
                className="animate-fade-in text-sm text-danger"
              >
                {t("updates.error", { error: String(checkUpdates.error) })}
              </span>
            )}
          </div>
          <p className="text-sm text-muted-foreground leading-relaxed">
            {t("updates.description")}
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
