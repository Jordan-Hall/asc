// A repository's main language, guessed from the names in its root directory. asc has no language
// statistics RPC yet, so the dashboard uses what one cheap tree read already shows: a manifest file
// (Cargo.toml, go.mod, ...) decides, then the most common source extension.

export interface Language {
  name: string;
  /** Dot colour (decorative; the name is always shown beside it). */
  color: string;
}

const L = {
  rust: { name: "Rust", color: "#dea584" },
  go: { name: "Go", color: "#00add8" },
  ts: { name: "TypeScript", color: "#3178c6" },
  js: { name: "JavaScript", color: "#f1e05a" },
  py: { name: "Python", color: "#3572a5" },
  cs: { name: "C#", color: "#178600" },
  java: { name: "Java", color: "#b07219" },
  kotlin: { name: "Kotlin", color: "#a97bff" },
  ruby: { name: "Ruby", color: "#701516" },
  php: { name: "PHP", color: "#4f5d95" },
  cpp: { name: "C++", color: "#f34b7d" },
  c: { name: "C", color: "#555555" },
  swift: { name: "Swift", color: "#f05138" },
  shell: { name: "Shell", color: "#89e051" },
} as const satisfies Record<string, Language>;

const MANIFESTS: readonly [RegExp, Language][] = [
  [/^Cargo\.toml$/, L.rust],
  [/^go\.mod$/, L.go],
  [/^tsconfig(\..+)?\.json$/, L.ts],
  [/^(pyproject\.toml|requirements\.txt|setup\.py|uv\.lock)$/, L.py],
  [/\.(sln|csproj)$/, L.cs],
  [/^(pom\.xml|build\.gradle(\.kts)?)$/, L.java],
  [/^Gemfile$/, L.ruby],
  [/^composer\.json$/, L.php],
  [/^CMakeLists\.txt$/, L.cpp],
  [/^Package\.swift$/, L.swift],
  [/^package\.json$/, L.js],
];

const EXTENSIONS: Record<string, Language> = {
  rs: L.rust,
  go: L.go,
  ts: L.ts,
  tsx: L.ts,
  js: L.js,
  jsx: L.js,
  mjs: L.js,
  py: L.py,
  cs: L.cs,
  java: L.java,
  kt: L.kotlin,
  rb: L.ruby,
  php: L.php,
  cc: L.cpp,
  cpp: L.cpp,
  c: L.c,
  h: L.c,
  swift: L.swift,
  sh: L.shell,
};

export function detectLanguage(names: readonly string[]): Language | undefined {
  for (const [re, lang] of MANIFESTS) if (names.some((n) => re.test(n))) return lang;
  const tally = new Map<Language, number>();
  for (const n of names) {
    const lang = EXTENSIONS[n.slice(n.lastIndexOf(".") + 1).toLowerCase()];
    if (lang && n.includes(".")) tally.set(lang, (tally.get(lang) ?? 0) + 1);
  }
  let best: Language | undefined;
  let max = 0;
  for (const [lang, n] of tally) {
    if (n > max) {
      best = lang;
      max = n;
    }
  }
  return best;
}
