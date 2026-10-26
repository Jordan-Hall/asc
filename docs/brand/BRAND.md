# ASC brand kit

## Name
- **ASC**: Agent Source Control. Say "ASC" in prose; spell out "Agent Source Control" on first use.
- **ASC Hub**: the hosted product. Write "ASC Hub" with a space; never "ASCHub" or "ASC-Hub".
- Command-line tool and code identifiers use the lowercase form `asc`.

## Tagline
**Source control for people and their agents.** Use it as written, sentence case, with a full stop only in running text.

## Meaning of the mark
An abstract "A" made of a branching path. Two legs rise from separate starting points (people and agents) and converge at one node at the apex, the merged change. The crossbar is a single change committed across both paths. The rounded tile keeps continuity with the earlier glyph. The shape uses only strokes and one dot, so it holds at 16 px.

## Files
| File | Use |
| --- | --- |
| `logo-mark.svg` | Primary mark, 28x28 viewBox (header glyph size) |
| `logo-mark-mono.svg` | One-colour mark, `currentColor`, glyph cut out of the tile |
| `wordmark-light.svg` / `wordmark-dark.svg` | Mark + "ASC" for light / dark backgrounds |
| `wordmark-hub-light.svg` / `wordmark-hub-dark.svg` | Mark + "ASC Hub" |
| `favicon.svg`, `favicon-32.png`, `favicon-16.png` | Browser tab |
| `apple-touch-icon.png` | 180 px, full-bleed (the OS rounds it) |
| `og-image.png` | 1200x630 social card with tagline |
| `social-avatar.png` | 512 px, full-bleed square |

Wordmark letters are drawn as outlines, so they need no font. The OG image tagline is set in Inter (falls back to the system sans) and Mono in JetBrains Mono (falls back to the system mono). Re-render it on a machine with both fonts installed for exact type.

## Colour tokens
Contrast ratios are WCAG 2.x against the stated background.

| Token | Light | Dark | Notes |
| --- | --- | --- | --- |
| `--asc-primary` | `#125469` | `#2394b2` | Brand colour, links, tile. 8.41:1 on `#ffffff`; 5.31:1 on `#0b1220` |
| `--asc-ink` | `#0b1220` | `#f1f5f9` | Body and wordmark text. 18.72:1 on white; 17.09:1 on `#0b1220` |
| `--asc-bg` | `#ffffff` | `#0b1220` | Page background (dark = header colour) |
| `--asc-accent` | `#22d3ee` | `#22d3ee` | Highlights, node, focus. Dark only for text: 10.36:1 on `#0b1220`. On light use as fill only, never as text |
| `--asc-muted` | `#475569` | `#94a3b8` | Secondary text. 7.58:1 on white; 7.30:1 on `#0b1220` |

Mark colours: light tile `#125469`, legs `#ffffff`, accent `#22d3ee` (4.65:1 on the tile). Dark tile `#2394b2`, legs `#0b1220`, accent `#ffffff` (5.31:1).

Type: Inter for text, JetBrains Mono for code, commands and the "Agent Source Control" descriptor.

## Clear space and minimum size
- Clear space on every side: the width of the apex node (about 0.1 of the mark's height, 2.7 units of 28), and never less than 4 px. For the wordmark use 0.5x the mark height.
- Minimum mark size: 16 px (use `favicon.svg`, which has slightly heavier strokes). Minimum wordmark height: 24 px; below that use the mark alone.
- Minimum print size: 8 mm for the mark.

## Do
- Use the supplied files; pick the light or dark variant by background.
- Use the mono mark in a single colour (ink or primary) when colour is not available.
- Keep the tile corner radius at 25% of the side.

## Don't
- Do not recolour the legs or accent outside the tokens, add gradients, shadows or outlines.
- Do not rotate, skew, stretch or re-space the wordmark.
- Do not place the light mark on a busy image or on a background closer than 3:1 to the tile.
- Do not set the name in lowercase or all caps in a different style ("Asc", "A.S.C.").
- Do not use the accent cyan for text on a light background.

## Voice
Plain, precise and calm. Speak to engineers. Lead with what the thing does, then why. Short sentences, active voice, concrete nouns (change, branch, agent, review). Treat people and agents as equal collaborators. No hype, no exclamation marks, no jokes at the expense of errors. Example: "ASC records every change, who or what made it, and why."
