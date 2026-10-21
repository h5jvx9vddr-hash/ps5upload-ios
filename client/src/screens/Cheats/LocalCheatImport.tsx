import { useState } from "react";
import { Button } from "../../components";
import { Modal } from "../../components/Modal";
import { pickPath } from "../../lib/pickPath";
import { importLocalCheat } from "../../api/consoleFiles";
import { useTr } from "../../state/lang";

export function LocalCheatImport({
  host,
  onImported,
}: {
  host: string;
  onImported: () => void;
}) {
  const tr = useTr();
  const [path, setPath] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const pick = async () => {
    setError(null);
    setSaved(null);
    try {
      const selected = await pickPath({
        mode: "file",
        title: tr("cheats_import", undefined, "Import local JSON cheat"),
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (selected) setPath(selected);
    } catch (e) {
      setError(String(e));
    }
  };
  const run = async () => {
    if (!path || busy) return;
    setBusy(true);
    setError(null);
    try {
      const result = await importLocalCheat(host, path);
      setSaved(result.path);
      setPath(null);
      onImported();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <Button
        size="sm"
        variant="secondary"
        onClick={() => void pick()}
        disabled={busy}
      >
        {tr("cheats_import", undefined, "Import local JSON cheat")}
      </Button>
      {saved && (
        <span className="break-all text-xs" role="status">
          {tr(
            "cheats_import_saved",
            { path: saved },
            `Imported with toggles off: ${saved}`,
          )}
        </span>
      )}
      {!path && error && (
        <span role="alert" className="text-xs text-[var(--color-bad)]">
          {error}
        </span>
      )}
      {path && (
        <Modal
          open
          title={tr("cheats_import", undefined, "Import local JSON cheat")}
          onClose={() => {
            if (!busy) setPath(null);
          }}
        >
          <div className="space-y-3 text-sm">
            <p className="break-all font-mono">{path}</p>
            <p>
              {tr(
                "cheats_import_hint",
                undefined,
                "Use a JSON trainer up to 256 KiB, named for its exact title ID and game version (for example PPSA13197_01.000.json). Renaming a file does not make its patches compatible. SHN/MC4 must be converted to JSON first.",
              )}
            </p>
            <p>
              {tr(
                "cheats_import_state",
                undefined,
                "Turn the cheat engine off first. Titles with existing cheats or saved toggle state must have their cheats removed before import. Existing files are never replaced. Imported toggles stay off; enable them yourself only for the matching game version.",
              )}
            </p>
            {error && (
              <p role="alert" className="text-[var(--color-bad)]">
                {error}
              </p>
            )}
            <div className="flex justify-end gap-2">
              <Button
                variant="secondary"
                disabled={busy}
                onClick={() => setPath(null)}
              >
                {tr("cancel", undefined, "Cancel")}
              </Button>
              <Button disabled={busy} onClick={() => void run()}>
                {tr("playlists_import", undefined, "Import")}
              </Button>
            </div>
          </div>
        </Modal>
      )}
    </>
  );
}
