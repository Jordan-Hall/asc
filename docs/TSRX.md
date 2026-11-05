# TSRX in asc

[TSRX](https://tsrx.dev) (`.tsrx`) is a TypeScript language extension for declarative UI:
`@{ … }` component bodies, `@if`/`@for`/`@switch`/`@try` control flow, elements with `{expr}`
slots, and `<style>` bodies. This page lists what asc does with it and how to run the TSRX tools
in a workflow.

## What asc supports

| Where | What | Source |
| --- | --- | --- |
| Code intelligence | Symbols, tags (`component`, `scope`), imports, hunk scopes | `crates/asc-codeintel/src/tsrx.rs`; SPEC section 23.6 |
| Web code view | Highlighting with a line tokenizer: linear time, state carried across lines, lines over 20000 characters shown plain | `web/src/lib/highlight/tsrx.ts` |
| VS Code | TextMate grammar `source.tsrx`, embedded TypeScript and CSS, bracket and comment configuration | `plugins/vscode/syntaxes/tsrx.tmLanguage.json` |
| JetBrains IDEs | The same grammar, given to the bundled TextMate plugin as a bundle | `plugins/jetbrains/src/main/kotlin/asc/asc/tsrx/` |
| Workflows | The `tsrx-check` template: Prettier, ESLint and type checks on every change that touches a `.tsrx` file | `crates/asc-core/templates/workflows/tsrx-check.toml` |

The asc grammars only colour code. For diagnostics, completion, formatting in the editor and
go-to-definition, use the TSRX language server (next section) next to the asc plugin.

## Language server

`@tsrx/language-server` implements LSP for `.tsrx` with Volar and TypeScript. It also formats
with the project's Prettier and `@tsrx/prettier-plugin`.

```sh
npm install --global @tsrx/language-server
tsrx-language-server --stdio
```

- **VS Code:** install the TSRX Syntax extension (`TSRX.tsrx-vscode-plugin`), which starts the
  server. The asc extension starts no language server, so the two do not compete; both
  contribute a `tsrx` grammar, and either one colours the file.
- **JetBrains:** with LSP support (2023.2 or later, or the LSP4IJ plugin), add a server with the
  command `tsrx-language-server --stdio` for `*.tsrx` files and the language ID `tsrx`.
- **Other editors:** use any LSP client with the same command and language ID.
- **Other TypeScript servers:** add `@tsrx/typescript-plugin` to `compilerOptions.plugins` in
  `tsconfig.json`.

## Formatting, linting and type checks in a workflow

Write the template into the repository, review it and land it:

```sh
asc workflow templates            # lists the built-in templates
asc workflow new tsrx-check       # writes .asc/workflows/tsrx-check.toml
asc workflow validate tsrx-check -F .asc/workflows/tsrx-check.toml
```

The workflow runs once the file is on trunk. It starts when a change is opened or updated and
touches a `.tsrx` file or a tool configuration file. You can also start it by hand with
`asc workflow run tsrx-check`. It has three `command` steps, which run side by side, each in a
fresh working copy:

| Step | Command after `npm ci --ignore-scripts` |
| --- | --- |
| `format` | `prettier --check '**/*.tsrx'` |
| `lint` | `eslint --max-warnings 0 .` |
| `types` | `tsrx-tsc --noEmit` |

The repository must declare the tools as dev dependencies and commit `package-lock.json`:

```sh
npm install -D prettier @tsrx/prettier-plugin eslint @tsrx/eslint-parser @tsrx/eslint-plugin \
  typescript @tsrx/typescript-plugin
```

`.prettierrc`:

```json
{ "plugins": ["@tsrx/prettier-plugin"] }
```

`eslint.config.js`:

```js
import tsrx from "@tsrx/eslint-plugin";

export default [...tsrx.configs.recommended];
```

Notes:

- Install scripts do not run (`--ignore-scripts`). A step does not read secrets, so it does not
  wait for the change to be approved. A change from anyone with write access runs the checks.
- The sandbox must reach the npm registry. Point npm at a mirror in `.npmrc` if the operator
  allows only that host.
- Each step reports a check on the change named `<workflow>/<step>` (`tsrx-check/format`,
  `tsrx-check/lint`, `tsrx-check/types`), so a branch rule can require them.
- For pnpm or bun, change the command in each step, for example
  `pnpm install --frozen-lockfile --ignore-scripts && pnpm exec prettier --check '**/*.tsrx'`.
- For another package manager, or to add a step, edit the file. It is an ordinary workflow (see
  `crates/asc-core/src/workflow.rs` for the schema).
