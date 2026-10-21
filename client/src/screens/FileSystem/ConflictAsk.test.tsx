import { isValidElement, type ReactElement, type ReactNode } from "react";
import { describe, expect, it, vi } from "vitest";

vi.mock("../../state/lang", () => ({
  useTr: () =>
    (key: string, vars?: Record<string, string | number>, fallback?: string) => {
      let s = fallback ?? key;
      for (const [k, v] of Object.entries(vars ?? {})) s = s.replace(`{${k}}`, String(v));
      return s;
    },
}));

import { ConflictPanel, conflictDecision } from "./ConflictAsk";
import type { FsConflict, FsConflictChoice } from "../../state/fsUploadPlan";

type El = ReactElement<Record<string, unknown>>;

/** Every element of the panel's tree (function components other than the panel are not
 *  rendered: a <Button>'s props are what matter here). */
function walk(node: ReactNode, out: El[] = []): El[] {
  if (Array.isArray(node)) {
    for (const n of node) walk(n, out);
  } else if (isValidElement(node)) {
    const el = node as El;
    out.push(el);
    walk(el.props.children as ReactNode, out);
  }
  return out;
}

function render(c: Partial<FsConflict>, all = false) {
  const settled: (FsConflictChoice | null)[] = [];
  const allSet: boolean[] = [];
  const conflict: FsConflict = {
    name: "a.bin",
    pickedDir: false,
    existingDir: false,
    canReplace: true,
    left: 0,
    ...c,
  };
  const tree = ConflictPanel({
    conflict,
    all,
    onAll: (v) => allSet.push(v),
    onSettle: (ch) => settled.push(ch),
  }) as El;
  const els = walk(tree);
  const buttons = els.filter((e) => typeof e.props.onClick === "function" && e.props.size === "sm");
  const button = (label: string) => buttons.find((b) => b.props.children === label);
  return { tree, els, buttons, button, settled, allSet };
}

describe("ConflictPanel", () => {
  it("focuses Keep both for a file, never Replace", () => {
    const r = render({});
    const focused = r.buttons.filter((b) => b.props.autoFocus);
    expect(focused.map((b) => b.props.children)).toEqual(["Keep both"]);
    expect(r.button("Replace")?.props.variant).toBe("danger");
  });

  it("focuses Keep both for a folder onto a folder too, never Merge", () => {
    const r = render({ pickedDir: true, existingDir: true });
    expect(r.buttons.filter((b) => b.props.autoFocus).map((b) => b.props.children)).toEqual([
      "Keep both",
    ]);
    expect(r.button("Merge")).toBeDefined();
    expect(r.button("Merge")?.props.autoFocus).toBeFalsy();
  });

  it("offers no Replace where a file meets a folder", () => {
    const r = render({ canReplace: false, existingDir: true });
    expect(r.buttons.map((b) => b.props.children)).toEqual(["Cancel", "Skip", "Keep both"]);
  });

  it("a click on the scrim cancels; a click inside the panel does not reach it", () => {
    const r = render({});
    (r.tree.props.onClick as () => void)();
    expect(r.settled).toEqual([null]);
    const panel = r.els.find((e) => e.props["data-testid"] === "fs-conflict")!;
    let stopped = false;
    (panel.props.onClick as (e: { stopPropagation: () => void }) => void)({
      stopPropagation: () => (stopped = true),
    });
    expect(stopped).toBe(true);
  });

  it("each button settles its own answer", () => {
    const r = render({});
    for (const label of ["Cancel", "Skip", "Keep both", "Replace"])
      (r.button(label)!.props.onClick as () => void)();
    expect(r.settled).toEqual([null, "skip", "keep", "replace"]);
  });

  it("the apply-to-all box shows only when more conflicts follow, with their count", () => {
    expect(render({ left: 0 }).els.some((e) => e.props["data-testid"] === "fs-conflict-all")).toBe(false);
    const r = render({ left: 3 });
    const box = r.els.find((e) => e.props["data-testid"] === "fs-conflict-all")!;
    (box.props.onChange as (e: { target: { checked: boolean } }) => void)({ target: { checked: true } });
    expect(r.allSet).toEqual([true]);
    const label = r.els.find((e) => e.type === "label")!;
    expect([label.props.children].flat()).toContain("Do the same for the other 3 conflicts");
  });
});

describe("conflictDecision", () => {
  it("Cancel (Escape, scrim, the button) is null whatever the box says", () => {
    expect(conflictDecision(null, true, 5)).toBeNull();
  });

  it("apply to all is clamped off when no conflict follows", () => {
    expect(conflictDecision("skip", true, 0)).toEqual({ choice: "skip", all: false });
    expect(conflictDecision("keep", true, 2)).toEqual({ choice: "keep", all: true });
    expect(conflictDecision("replace", false, 2)).toEqual({ choice: "replace", all: false });
  });
});
