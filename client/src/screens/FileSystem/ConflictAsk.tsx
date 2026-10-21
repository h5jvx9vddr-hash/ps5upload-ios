import { useEffect, useId, useRef, useState, type RefObject } from "react";
import { AlertTriangle } from "lucide-react";

import { Button } from "../../components";
import { activateFocusTrap } from "../../lib/focusTrap";
import { useTr } from "../../state/lang";
import type {
  FsConflict,
  FsConflictChoice,
  FsConflictDecision,
} from "../../state/fsUploadPlan";

/**
 * The question a copy onto the PS5 asks when a name is already in the folder: Skip, Keep both,
 * or Replace (Merge for a folder into a folder), with "do the same for the other N" when more
 * conflicts follow. Asked BEFORE the upload starts (state/fsUploadPlan), so the answers travel
 * with the one job. Same imperative shape as `useConfirm`: `await ask(conflict)`; null = Cancel
 * (nothing is copied). Escape, a click on the scrim and Cancel all answer null.
 */

/** The answer for a button press: null (Cancel) stays null; "for all" only counts when later
 *  conflicts exist. */
export function conflictDecision(
  choice: FsConflictChoice | null,
  all: boolean,
  left: number,
): FsConflictDecision | null {
  return choice ? { choice, all: all && left > 0 } : null;
}

export interface ConflictPanelProps {
  conflict: FsConflict;
  all: boolean;
  onAll: (all: boolean) => void;
  onSettle: (choice: FsConflictChoice | null) => void;
  titleId?: string;
  bodyId?: string;
  panelRef?: RefObject<HTMLDivElement | null>;
}

/** The dialog itself, stateless (the hook below owns the state). Keep both always has the focus:
 *  Enter must never overwrite or merge unasked. */
export function ConflictPanel({
  conflict,
  all,
  onAll,
  onSettle,
  titleId,
  bodyId,
  panelRef,
}: ConflictPanelProps) {
  const tr = useTr();
  const folderMerge = conflict.canReplace && conflict.pickedDir;
  const body = !conflict.canReplace
    ? tr(
        "fs_conflict_body_mixed",
        undefined,
        "A file and a folder can't replace each other. Skip it, or keep both under a new name.",
      )
    : folderMerge
      ? tr(
          "fs_conflict_body_folder",
          undefined,
          "Merge copies into the folder that is there: files with the same name are overwritten, everything else in it is left alone.",
        )
      : tr(
          "fs_conflict_body_file",
          undefined,
          "Replace overwrites the file that is there, and that can't be undone. If this is inside a mounted game image, a bad file can stop the game booting.",
        );
  return (
    <div
      className="anim-scrim scrim fixed inset-0 z-[60] flex items-center justify-center p-4"
      onClick={() => onSettle(null)}
      data-testid="fs-conflict-scrim"
    >
      <div
        ref={panelRef}
        role="alertdialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={bodyId}
        className="anim-pop glass-float elev-3 w-full max-w-md rounded-[var(--radius-panel)] p-6"
        onClick={(e) => e.stopPropagation()}
        data-testid="fs-conflict"
      >
        <header
          id={titleId}
          className="mb-2 flex items-center gap-2 text-base font-semibold"
        >
          <AlertTriangle size={14} className="text-[var(--color-warn)]" />
          <span className="min-w-0 break-words">
            {tr(
              "fs_conflict_title",
              { name: conflict.name },
              `"${conflict.name}" is already here`,
            )}
          </span>
        </header>
        <p
          id={bodyId}
          className="mb-4 whitespace-pre-line text-sm text-[var(--color-muted)]"
        >
          {body}
        </p>
        {conflict.left > 0 && (
          <label className="mb-4 flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={all}
              onChange={(e) => onAll(e.target.checked)}
              data-testid="fs-conflict-all"
            />
            {tr(
              "fs_conflict_apply_all",
              { count: conflict.left },
              `Do the same for the other ${conflict.left} conflicts`,
            )}
          </label>
        )}
        <div className="flex flex-wrap items-center justify-end gap-2">
          <Button variant="ghost" size="sm" onClick={() => onSettle(null)}>
            {tr("cancel", undefined, "Cancel")}
          </Button>
          <Button variant="secondary" size="sm" onClick={() => onSettle("skip")}>
            {tr("fs_conflict_skip", undefined, "Skip")}
          </Button>
          <Button
            variant="primary"
            size="sm"
            onClick={() => onSettle("keep")}
            autoFocus
          >
            {tr("fs_conflict_keep_both", undefined, "Keep both")}
          </Button>
          {conflict.canReplace && (
            <Button
              variant={folderMerge ? "secondary" : "danger"}
              size="sm"
              onClick={() => onSettle("replace")}
            >
              {folderMerge
                ? tr("fs_conflict_merge", undefined, "Merge")
                : tr("fs_conflict_replace", undefined, "Replace")}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}

/** `resetKey` (the console): when it changes, or the screen goes away, a question still open is
 *  answered Cancel, so the pick waiting on it is dropped instead of hanging. */
export function useConflictAsk(resetKey?: string): {
  ask: (c: FsConflict) => Promise<FsConflictDecision | null>;
  dialog: React.ReactNode;
} {
  const [pending, setPending] = useState<
    (FsConflict & { resolve: (d: FsConflictDecision | null) => void }) | null
  >(null);
  const [all, setAll] = useState(false);
  const titleId = useId();
  const bodyId = useId();
  const panelRef = useRef<HTMLDivElement | null>(null);
  // The open question, for the cleanup below (state is not readable there).
  const pendingRef = useRef<typeof pending>(null);

  useEffect(
    () => () => {
      pendingRef.current?.resolve(null);
      pendingRef.current = null;
      setPending(null);
    },
    [resetKey],
  );

  const ask = (c: FsConflict) =>
    new Promise<FsConflictDecision | null>((resolve) => {
      const next = { ...c, resolve };
      pendingRef.current = next;
      setAll(false);
      setPending(next);
    });

  const settle = (choice: FsConflictChoice | null) => {
    const p = pendingRef.current;
    if (!p) return;
    pendingRef.current = null;
    p.resolve(conflictDecision(choice, all, p.left));
    setPending(null);
  };

  useEffect(() => {
    if (!pending) return;
    return activateFocusTrap(() => panelRef.current, window, {
      onEscape: (e) => {
        e.preventDefault();
        settle(null);
      },
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pending]);

  return {
    ask,
    dialog: pending ? (
      <ConflictPanel
        conflict={pending}
        all={all}
        onAll={setAll}
        onSettle={settle}
        titleId={titleId}
        bodyId={bodyId}
        panelRef={panelRef}
      />
    ) : null,
  };
}
