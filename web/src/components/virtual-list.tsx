// A fixed-row-height virtual list with roving keyboard focus (arrows, j and k, Home, End). Only the
// visible rows are in the DOM. Rows are absolutely positioned inside a spacer of the full height.
import { type KeyboardEvent, type ReactNode, useEffect, useRef, useState } from "react";
import { useVirtualRows } from "../lib/virtual";

export interface VirtualListProps {
  count: number;
  rowHeight: number;
  /** Rows shown before the list scrolls (its height is `min(count, maxRows) * rowHeight`). */
  maxRows: number;
  label: string;
  renderRow: (index: number, focusable: boolean) => ReactNode;
  /** Extra class for the scroll container. */
  className?: string;
  /** Reserve this many rows of height even when there are fewer (no shift while streaming). */
  minRows?: number;
  /** Scroll this row into view once it exists (a #L12 anchor). */
  revealIndex?: number | undefined;
}

export function VirtualList(props: VirtualListProps): ReactNode {
  const { count, rowHeight, maxRows, label, renderRow } = props;
  const rows = useVirtualRows(count, rowHeight);
  const host = useRef<HTMLDivElement | null>(null);
  const [active, setActive] = useState(0);
  const moved = useRef(false);

  const height = Math.min(Math.max(count, props.minRows ?? 0), maxRows) * rowHeight;
  const current = Math.min(active, Math.max(0, count - 1));

  const reveal = props.revealIndex;
  const revealReady = reveal !== undefined && reveal >= 0 && reveal < count;
  const scrollToIndex = rows.scrollToIndex;
  useEffect(() => {
    if (revealReady && reveal !== undefined) scrollToIndex(reveal);
  }, [revealReady, reveal, scrollToIndex]);

  // After keyboard movement, focus the row once it has been rendered.
  useEffect(() => {
    if (!moved.current) return;
    moved.current = false;
    host.current
      ?.querySelector<HTMLElement>(
        `[data-row="${current}"] [tabindex="0"], [data-row="${current}"] a`,
      )
      ?.focus();
  }, [current, rows.range.start, rows.range.end]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    const page = Math.max(1, maxRows - 1);
    const next =
      e.key === "ArrowDown" || e.key === "j"
        ? current + 1
        : e.key === "ArrowUp" || e.key === "k"
          ? current - 1
          : e.key === "PageDown"
            ? current + page
            : e.key === "PageUp"
              ? current - page
              : e.key === "Home"
                ? 0
                : e.key === "End"
                  ? count - 1
                  : undefined;
    if (next === undefined || count === 0) return;
    e.preventDefault();
    const clamped = Math.max(0, Math.min(count - 1, next));
    moved.current = true;
    setActive(clamped);
    rows.scrollToIndex(clamped);
  };

  const items: ReactNode[] = [];
  for (let i = rows.range.start; i < rows.range.end; i++) {
    items.push(
      <div
        key={i}
        role="listitem"
        data-row={i}
        className="absolute inset-x-0 border-b border-line"
        style={{ top: i * rowHeight, height: rowHeight }}
      >
        {renderRow(i, i === current)}
      </div>,
    );
  }

  return (
    <div
      ref={(el) => {
        host.current = el;
        rows.ref(el);
      }}
      role="list"
      aria-label={label}
      onScroll={rows.onScroll}
      onKeyDown={onKeyDown}
      className={`relative overflow-auto ${props.className ?? ""}`}
      style={{ height }}
    >
      <div style={{ height: rows.totalHeight, position: "relative", minWidth: "100%" }}>
        {items}
      </div>
    </div>
  );
}
