import { useEffect, useState } from "react";
import { Button } from "../../components";
import { minionTile, type MinionTileState } from "../../api/consoleFiles";
import { useConnectionStore } from "../../state/connection";
import { useTr } from "../../state/lang";

export function MinionSettings() {
  const host = useConnectionStore((s) => s.host);
  const up = useConnectionStore((s) => s.payloadStatus === "up");
  return <MinionTileControl key={`${host}:${up}`} host={host} up={up} />;
}

function MinionTileControl({ host, up }: { host: string; up: boolean }) {
  const tr = useTr();
  const [state, setState] = useState<MinionTileState | null>(null);
  const [busy, setBusy] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!host || !up || busy) return;
    let gone = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try {
        const s = await minionTile(host);
        if (gone) return;
        setState(s);
        if (s.supported && s.tile_busy)
          timer = setTimeout(() => void poll(), 1500);
      } catch (e) {
        if (!gone) setError(String(e));
      }
    };
    void poll();
    return () => {
      gone = true;
      clearTimeout(timer);
    };
  }, [host, up, busy, attempt]);
  const apply = async (tile: boolean) => {
    setBusy(true);
    setError(null);
    try {
      setState(await minionTile(host, tile));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="space-y-3 text-sm">
      <p>
        {tr(
          "cw_settings_tile",
          undefined,
          "Show or hide Minion on the PS5 home screen using the paired helper connection.",
        )}
      </p>
      {!up && (
        <p>
          {tr(
            "minion_tile_connect",
            undefined,
            "Connect to the helper to change Minion visibility.",
          )}
        </p>
      )}
      {state?.supported === false && (
        <p>
          {tr(
            "minion_tile_update",
            undefined,
            "Update the helper to use this control.",
          )}
        </p>
      )}
      {state?.supported && (
        <>
          <p>
            {state.tile_busy
              ? tr(
                  "cw_files_perm_applying",
                  undefined,
                  "Updating the home screen…",
                )
              : state.tile_failed
                ? tr(
                    state.tile
                      ? "cw_settings_err_tile_add"
                      : "cw_settings_err_tile_remove",
                    undefined,
                    "The home screen update failed. Try again.",
                  )
                : state.tile
                  ? tr(
                      "cw_settings_on",
                      undefined,
                      "Minion is set to appear on the home screen.",
                    )
                  : tr(
                      "cw_settings_off",
                      undefined,
                      "Minion is set to stay hidden.",
                    )}
          </p>
          <div className="flex flex-wrap gap-2">
            <Button
              size="sm"
              disabled={busy || state.tile_busy}
              onClick={() => void apply(true)}
            >
              {tr("minion_tile_show", undefined, "Show / restore Minion")}
            </Button>
            <Button
              size="sm"
              variant="secondary"
              disabled={busy || state.tile_busy}
              onClick={() => void apply(false)}
            >
              {tr("minion_tile_hide", undefined, "Hide Minion")}
            </Button>
          </div>
        </>
      )}
      {error && (
        <div>
          <p role="alert" className="text-[var(--color-bad)]">
            {error}
          </p>
          <Button
            size="sm"
            disabled={busy}
            onClick={() => {
              setError(null);
              setAttempt((n) => n + 1);
            }}
          >
            {tr("retry", undefined, "Retry")}
          </Button>
        </div>
      )}
    </div>
  );
}
