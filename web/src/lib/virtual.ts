// Fixed-row-height windowing. The DOM holds only the visible rows plus overscan, however long the
// list is (docs/WEB-UI.md section 5). `windowRange` is pure so it can be tested without a DOM.
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";

export interface Range {
  /** First row index to render. */
  start: number;
  /** One past the last row index to render. */
  end: number;
}

export function windowRange(
  scrollTop: number,
  viewport: number,
  rowHeight: number,
  count: number,
  overscan = 8,
): Range {
  if (count <= 0 || rowHeight <= 0) return { start: 0, end: 0 };
  const first = Math.min(count - 1, Math.floor(Math.max(0, scrollTop) / rowHeight));
  const visible = Math.ceil(Math.max(0, viewport) / rowHeight) + 1;
  const start = Math.max(0, first - overscan);
  const end = Math.min(count, first + visible + overscan);
  return { start, end: Math.max(start, end) };
}

export interface VirtualRows {
  ref: (el: HTMLElement | null) => void;
  onScroll: () => void;
  range: Range;
  totalHeight: number;
  /** Scrolls so that row `index` is in view. */
  scrollToIndex: (index: number) => void;
}

const useIsoLayoutEffect = typeof window === "undefined" ? useEffect : useLayoutEffect;

export function useVirtualRows(count: number, rowHeight: number, overscan = 8): VirtualRows {
  const el = useRef<HTMLElement | null>(null);
  const frame = useRef(0);
  const [state, setState] = useState({ top: 0, height: 600 });

  const measure = useCallback(() => {
    const node = el.current;
    if (!node) return;
    setState((s) =>
      s.top === node.scrollTop && s.height === node.clientHeight
        ? s
        : { top: node.scrollTop, height: node.clientHeight },
    );
  }, []);

  const onScroll = useCallback(() => {
    if (frame.current) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      measure();
    });
  }, [measure]);

  const ref = useCallback(
    (node: HTMLElement | null) => {
      el.current = node;
      if (node) measure();
    },
    [measure],
  );

  useIsoLayoutEffect(() => {
    const node = el.current;
    if (!node || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(measure);
    ro.observe(node);
    return () => ro.disconnect();
  }, [measure]);

  const scrollToIndex = useCallback(
    (index: number) => {
      const node = el.current;
      if (!node) return;
      const top = index * rowHeight;
      if (top < node.scrollTop) node.scrollTop = top;
      else if (top + rowHeight > node.scrollTop + node.clientHeight)
        node.scrollTop = top + rowHeight - node.clientHeight;
    },
    [rowHeight],
  );

  return {
    ref,
    onScroll,
    range: windowRange(state.top, state.height, rowHeight, count, overscan),
    totalHeight: count * rowHeight,
    scrollToIndex,
  };
}
