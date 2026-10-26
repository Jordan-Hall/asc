// Placeholders with the real dimensions of what they stand in for, so nothing moves when data
// arrives (docs/WEB-UI.md section 4.5).
import type { ReactNode } from "react";

export function Bar(props: { width?: string; height?: number }): ReactNode {
  return (
    <div
      className="skeleton"
      style={{ width: props.width ?? "100%", height: props.height ?? 14 }}
      aria-hidden="true"
    />
  );
}

/** `rows` rows of `rowHeight` px, matching a virtual list. */
export function RowsSkeleton(props: { rows: number; rowHeight: number; label: string }): ReactNode {
  const items = Array.from({ length: props.rows }, (_, i) => i);
  return (
    <div role="status" aria-busy="true" aria-label={props.label}>
      {items.map((i) => (
        <div
          key={i}
          className="flex items-center gap-3 border-b border-line px-4"
          style={{ height: props.rowHeight }}
        >
          <Bar width="1rem" height={16} />
          <Bar width={`${30 + ((i * 17) % 40)}%`} />
        </div>
      ))}
    </div>
  );
}

export function PageSkeleton(): ReactNode {
  return (
    <div
      className="mx-auto min-h-dvh w-full max-w-6xl p-4"
      role="status"
      aria-busy="true"
      aria-label="Loading page"
    >
      <div className="mb-6" style={{ height: 32 }}>
        <Bar width="40%" height={28} />
      </div>
      <div className="rounded-lg border border-line bg-surface">
        <RowsSkeleton rows={8} rowHeight={36} label="Loading" />
      </div>
    </div>
  );
}
