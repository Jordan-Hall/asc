// The handful of icons the app draws, inline so there is no icon font and no extra request.
import type { ReactNode } from "react";

const paths = {
  folder:
    "M2 4.5A1.5 1.5 0 0 1 3.5 3h3l1.5 1.5h4.5A1.5 1.5 0 0 1 14 6v6a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 12z",
  file: "M4 2h5l3 3v8.5a.5.5 0 0 1-.5.5h-7a.5.5 0 0 1-.5-.5v-11A.5.5 0 0 1 4 2zM9 2v3h3",
  branch: "M5 3v10M5 6a2 2 0 1 0 0-.01M11 6a2 2 0 1 0 0-.01M11 8c0 2-6 1-6 4",
  tag: "M2.5 8.5V3.5h5l6 6-5 5zM5.5 6a.5.5 0 1 0 0-.01",
  sun: "M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM8 1v2M8 13v2M1 8h2M13 8h2M3 3l1.4 1.4M11.6 11.6 13 13M3 13l1.4-1.4M11.6 4.4 13 3",
  moon: "M13 9.5A5.5 5.5 0 0 1 6.5 3 5.5 5.5 0 1 0 13 9.5z",
  inbox: "M2 9l1.5-5.5h9L14 9v4H2zM2 9h3.5l1 1.5h3L11 9h3",
  search: "M7 12A5 5 0 1 0 7 2a5 5 0 0 0 0 10zM11 11l3.5 3.5",
  link: "M6.5 9.5l3-3M5 8l-1.5 1.5a2.1 2.1 0 0 0 3 3L8 11M11 8l1.5-1.5a2.1 2.1 0 0 0-3-3L8 5",
} as const;

export type IconName = keyof typeof paths;

export function Icon(props: { name: IconName; size?: number }): ReactNode {
  const size = props.size ?? 16;
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      <path d={paths[props.name]} />
    </svg>
  );
}
