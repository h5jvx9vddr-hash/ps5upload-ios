import { useState } from "react";
import { PS5_AVA1_PORT, PS5_LOADER_PORT } from "../lib/addr";
import { parsePort, useConsolePortsStore } from "../state/consolePorts";
import { useTr } from "../state/lang";

/**
 * The loader and helper port fields under the PS5 address (iOS fork). Each
 * field keeps a draft and commits on blur or Enter; an empty field goes back
 * to the default, an invalid one is flagged and not saved.
 */
export function ConsolePortsFields({ disabled }: { disabled?: boolean }) {
  const tr = useTr();
  const loaderPort = useConsolePortsStore((s) => s.loaderPort);
  const helperPort = useConsolePortsStore((s) => s.helperPort);
  const setLoaderPort = useConsolePortsStore((s) => s.setLoaderPort);
  const setHelperPort = useConsolePortsStore((s) => s.setHelperPort);
  return (
    <div className="mt-3 grid grid-cols-2 gap-2">
      <PortField
        label={tr("connection_loader_port", undefined, "Loader port")}
        hint={tr("connection_loader_port_hint", { port: PS5_LOADER_PORT }, "ELF loader, default {port}")}
        value={loaderPort}
        fallback={PS5_LOADER_PORT}
        onCommit={setLoaderPort}
        disabled={disabled}
      />
      <PortField
        label={tr("connection_helper_port", undefined, "Helper port")}
        hint={tr("connection_helper_port_hint", { port: PS5_AVA1_PORT }, "PS5Upload helper, default {port}")}
        value={helperPort}
        fallback={PS5_AVA1_PORT}
        onCommit={setHelperPort}
        disabled={disabled}
      />
    </div>
  );
}

function PortField({
  label,
  hint,
  value,
  fallback,
  onCommit,
  disabled,
}: {
  label: string;
  hint: string;
  value: number;
  fallback: number;
  onCommit: (port: number) => void;
  disabled?: boolean;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  const text = draft ?? String(value);
  const parsed = text.trim() === "" ? fallback : parsePort(text);
  const commit = () => {
    if (draft === null || parsed === null) return;
    if (parsed !== value) onCommit(parsed);
    setDraft(null);
  };
  return (
    <label className="block">
      <span className="block text-xs font-medium uppercase tracking-wide text-[var(--color-muted)]">
        {label}
      </span>
      <input
        value={text}
        onChange={(e) => setDraft(e.target.value.replace(/\D/g, ""))}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
        }}
        placeholder={String(fallback)}
        inputMode="numeric"
        disabled={disabled}
        aria-invalid={parsed === null}
        className={`input max-md:min-h-11 mt-1 w-full disabled:opacity-50 ${
          parsed === null ? "border-[var(--color-bad)]" : ""
        }`}
      />
      <span className="mt-1 block text-xs text-[var(--color-muted)]">{hint}</span>
    </label>
  );
}
