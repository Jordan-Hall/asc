// The icons the app draws, inline so there is no icon font and no extra request. 16 px grid,
// 1.4 stroke. Only this module's paths are paid for, once, in the entry chunk.
import type { ReactNode } from "react";

const paths = {
  folder:
    "M2 4.5A1.5 1.5 0 0 1 3.5 3h3l1.5 1.5h4.5A1.5 1.5 0 0 1 14 6v6a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 12z",
  file: "M4 2h5l3 3v8.5a.5.5 0 0 1-.5.5h-7a.5.5 0 0 1-.5-.5v-11A.5.5 0 0 1 4 2zM9 2v3h3",
  branch: "M5 3v10M5 6a2 2 0 1 0 0-.01M11 6a2 2 0 1 0 0-.01M11 8c0 2-6 1-6 4",
  tag: "M2.5 8.5V3.5h5l6 6-5 5zM5.5 6a.5.5 0 1 0 0-.01",
  sun: "M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM8 1v2M8 13v2M1 8h2M13 8h2M3 3l1.4 1.4M11.6 11.6 13 13M3 13l1.4-1.4M11.6 4.4 13 3",
  moon: "M13 9.5A5.5 5.5 0 0 1 6.5 3 5.5 5.5 0 1 0 13 9.5z",
  system: "M2 3.5h12v8H2zM5.5 14h5M8 11.5V14",
  inbox: "M2 9l1.5-5.5h9L14 9v4H2zM2 9h3.5l1 1.5h3L11 9h3",
  search: "M7 12A5 5 0 1 0 7 2a5 5 0 0 0 0 10zM11 11l3.5 3.5",
  repo: "M3 13.5V3.5A1.5 1.5 0 0 1 4.5 2H13v10H4.5A1.5 1.5 0 0 0 3 13.5zM3 13.5A1.5 1.5 0 0 0 4.5 15H13v-3",
  copy: "M5.5 5.5h8v8h-8zM10.5 5.5v-3h-8v8h3",
  link: "M6.5 9.5l3-3M5 8l-1.5 1.5a2.1 2.1 0 0 0 3 3L8 11M11 8l1.5-1.5a2.1 2.1 0 0 0-3-3L8 5",
  plus: "M8 3v10M3 8h10",
  chevronDown: "M4 6l4 4 4-4",
  chevronRight: "M6 4l4 4-4 4",
  arrowRight: "M3 8h10M9 4l4 4-4 4",
  check: "M3 8.5l3.2 3.2L13 4.8",
  x: "M4 4l8 8M12 4l-8 8",
  user: "M8 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM2.5 14c.4-2.6 2.6-4 5.5-4s5.1 1.4 5.5 4",
  gear: "M8 10.2a2.2 2.2 0 1 0 0-4.4 2.2 2.2 0 0 0 0 4.4zM8 1.5v1.8M8 12.7v1.8M1.5 8h1.8M12.7 8h1.8M3.4 3.4l1.3 1.3M11.3 11.3l1.3 1.3M3.4 12.6l1.3-1.3M11.3 4.7l1.3-1.3",
  keyboard: "M1.5 4h13v8h-13zM4 6.5h.01M6.5 6.5h.01M9 6.5h.01M11.5 6.5h.01M4.5 9.5h7",
  signout: "M6 2.5H3.5v11H6M10 5l3 3-3 3M13 8H6",
  menu: "M2.5 4.5h11M2.5 8h11M2.5 11.5h11",
  layers: "M8 2l6 3-6 3-6-3zM2 8l6 3 6-3M2 11l6 3 6-3",
  comment: "M2.5 3.5h11v7.5H8l-3 2.5V11H2.5z",
  bot: "M4 5.5h8a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1v-5a1 1 0 0 1 1-1zM8 5.5V3M6 9h.01M10 9h.01M1.5 8.5v1.5M14.5 8.5v1.5",
  terminal: "M2 3h12v10H2zM4.5 6.5l2 1.5-2 1.5M8 10h3.5",
  bolt: "M9 1.5L3.5 9H8l-1 5.5L12.5 7H8z",
  clock: "M8 14A6 6 0 1 0 8 2a6 6 0 0 0 0 12zM8 4.5V8l2.2 1.4",
  shield: "M8 1.5l5.5 2v4.2c0 3.2-2.3 5.6-5.5 6.8-3.2-1.2-5.5-3.6-5.5-6.8V3.5z",
  merge: "M4 3v10M12 7v2.5A2.5 2.5 0 0 1 9.5 12H4M12 7a2 2 0 1 0 0-.01",
  eye: "M1.5 8S4 3.5 8 3.5 14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8zM8 9.8a1.8 1.8 0 1 0 0-3.6 1.8 1.8 0 0 0 0 3.6z",
  alert: "M8 2l6.5 11.5h-13zM8 6.5v3M8 11.3h.01",
} as const;

export type IconName = keyof typeof paths;

export function Icon(props: { name: IconName; size?: number; className?: string }): ReactNode {
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
      className={props.className}
    >
      <path d={paths[props.name]} />
    </svg>
  );
}

/**
 * The ASC mark (docs/brand/logo-mark.svg): an "A" made of two paths that converge on one node.
 * `tone="light"` is the tile for a light background, `"dark"` the tile for an ink background (the
 * header, hero and closing band). Colours are the brand tokens, fixed, as BRAND.md requires.
 */
export function LogoMark(props: { size?: number; tone?: "light" | "dark" }): ReactNode {
  const size = props.size ?? 28;
  const dark = props.tone === "dark";
  return (
    <svg width={size} height={size} viewBox="0 0 28 28" aria-hidden="true" focusable="false">
      <rect width="28" height="28" rx="7" fill={dark ? "#2394b2" : "#125469"} />
      <path
        d="M7.5 22.2 14 6.6l6.5 15.6"
        fill="none"
        stroke={dark ? "#0b1220" : "#ffffff"}
        strokeWidth="3"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M10.4 17.3h7.2"
        fill="none"
        stroke={dark ? "#ffffff" : "#22d3ee"}
        strokeWidth="2.4"
        strokeLinecap="round"
      />
      <circle cx="14" cy="6.6" r="2.7" fill={dark ? "#ffffff" : "#22d3ee"} />
    </svg>
  );
}
