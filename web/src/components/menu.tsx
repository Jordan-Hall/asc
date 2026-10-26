// A dropdown menu button (WAI-ARIA menu button pattern): the trigger opens a `role="menu"`, arrow
// keys / Home / End move between items, a letter jumps to the next item starting with it, Escape
// closes and returns focus to the trigger, Tab and an outside press close it. Items are buttons,
// router links or plain anchors. It is absolutely positioned, so opening it never shifts the page.
import { type ReactNode, useEffect, useId, useRef, useState } from "react";
import { Link } from "../lib/router";
import { Icon, type IconName } from "./icons";

export type MenuEntry =
  | {
      kind: "item";
      id: string;
      label: string;
      icon?: IconName;
      /** Right-aligned shortcut or note. */
      hint?: string;
      /** In-app route. */
      to?: string;
      /** Same-page or external anchor. */
      href?: string;
      onSelect?: () => void;
      danger?: boolean;
    }
  | {
      kind: "radio";
      id: string;
      label: string;
      icon?: IconName;
      checked: boolean;
      onSelect: () => void;
    }
  | { kind: "heading"; id: string; label: string }
  | { kind: "separator"; id: string };

const ITEM = "[role='menuitem'],[role='menuitemradio']";
const ROW =
  "flex w-full min-h-11 items-center gap-2.5 sm:min-h-9 rounded-lg px-2.5 text-left text-sm no-underline " +
  "outline-none hover:bg-subtle-hover focus-visible:bg-subtle-hover focus-visible:ring-2 " +
  "focus-visible:ring-inset focus-visible:ring-[rgb(var(--color-text-link-focus-ring))]";

export function Menu(props: {
  /** Accessible name of the menu and its button. */
  label: string;
  /** Visible button content. */
  button: ReactNode;
  buttonClassName: string;
  align?: "start" | "end";
  entries: readonly MenuEntry[];
}): ReactNode {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement | null>(null);
  const list = useRef<HTMLDivElement | null>(null);
  const trigger = useRef<HTMLButtonElement | null>(null);
  const menuId = useId();

  const items = (): HTMLElement[] => [...(list.current?.querySelectorAll<HTMLElement>(ITEM) ?? [])];
  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) trigger.current?.focus();
  };

  useEffect(() => {
    if (!open) return;
    items()[0]?.focus();
    const onDown = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  }, [open]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    const all = items();
    const at = all.indexOf(document.activeElement as HTMLElement);
    const go = (i: number) => {
      e.preventDefault();
      all[(i + all.length) % all.length]?.focus();
    };
    switch (e.key) {
      case "ArrowDown":
        return go(at + 1);
      case "ArrowUp":
        return go(at < 0 ? all.length - 1 : at - 1);
      case "Home":
        return go(0);
      case "End":
        return go(all.length - 1);
      case "Escape":
        e.preventDefault();
        return close(true);
      case "Tab":
        return close(false);
      default: {
        if (e.key.length !== 1 || e.ctrlKey || e.metaKey || e.altKey) return;
        const ch = e.key.toLowerCase();
        const order = [...all.slice(at + 1), ...all.slice(0, at + 1)];
        const hit = order.find((el) => el.textContent?.trim().toLowerCase().startsWith(ch));
        if (hit) {
          e.preventDefault();
          hit.focus();
        }
      }
    }
  };

  return (
    <div ref={root} className="relative">
      <button
        ref={trigger}
        type="button"
        aria-label={props.label}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        className={props.buttonClassName}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" && !open) {
            e.preventDefault();
            setOpen(true);
          }
        }}
      >
        {props.button}
      </button>
      {open ? (
        <div
          ref={list}
          id={menuId}
          role="menu"
          aria-label={props.label}
          tabIndex={-1}
          onKeyDown={onKeyDown}
          className={`absolute top-full z-50 mt-2 w-max min-w-56 max-w-[calc(100vw-2rem)] rounded-xl border border-line bg-surface p-1.5 shadow-xl ${
            props.align === "start" ? "left-0" : "right-0"
          }`}
        >
          {props.entries.map((e) => {
            if (e.kind === "separator")
              return <div key={e.id} role="separator" className="my-1.5 h-px bg-line" />;
            if (e.kind === "heading")
              return (
                <div
                  key={e.id}
                  role="presentation"
                  className="px-2.5 pb-1 pt-1.5 text-xs font-semibold uppercase tracking-wide text-fg-muted"
                >
                  {e.label}
                </div>
              );
            if (e.kind === "radio")
              return (
                <button
                  key={e.id}
                  type="button"
                  role="menuitemradio"
                  aria-checked={e.checked}
                  tabIndex={-1}
                  className={ROW}
                  onClick={() => {
                    e.onSelect();
                    close(true);
                  }}
                >
                  <span className="flex w-4 justify-center text-accent">
                    {e.checked ? <Icon name="check" /> : e.icon ? <Icon name={e.icon} /> : null}
                  </span>
                  <span className="flex-1">{e.label}</span>
                </button>
              );
            const body = (
              <>
                <span
                  className={`flex w-4 justify-center ${e.danger ? "text-bad" : "text-fg-muted"}`}
                >
                  {e.icon ? <Icon name={e.icon} /> : null}
                </span>
                <span className={`flex-1 ${e.danger ? "text-bad" : "text-fg"}`}>{e.label}</span>
                {e.hint ? <span className="text-xs text-fg-muted">{e.hint}</span> : null}
              </>
            );
            if (e.to)
              return (
                <Link
                  key={e.id}
                  to={e.to}
                  role="menuitem"
                  tabIndex={-1}
                  className={ROW}
                  onClick={() => close(false)}
                >
                  {body}
                </Link>
              );
            if (e.href)
              return (
                <a
                  key={e.id}
                  href={e.href}
                  role="menuitem"
                  tabIndex={-1}
                  className={ROW}
                  onClick={() => close(false)}
                >
                  {body}
                </a>
              );
            return (
              <button
                key={e.id}
                type="button"
                role="menuitem"
                tabIndex={-1}
                className={ROW}
                onClick={() => {
                  close(false);
                  e.onSelect?.();
                }}
              >
                {body}
              </button>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
