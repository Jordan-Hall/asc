// The product page sections that the header menu and the footer link to. The agents section only
// exists when "Show agent features" is on (src/lib/agents.ts).
export interface Section {
  href: string;
  label: string;
}

export function sections(agents: boolean): readonly Section[] {
  return [
    { href: "#stacks", label: "Stacked changes" },
    { href: "#review", label: "Review" },
    { href: "#inbox", label: "Inbox" },
    { href: "#search", label: "Search" },
    ...(agents ? [{ href: "#agents", label: "Agents" }] : []),
    { href: "#clone", label: "Clone" },
  ];
}
