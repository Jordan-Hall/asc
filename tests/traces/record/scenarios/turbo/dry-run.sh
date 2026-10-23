#@ tool: turbo
#@ version: 2.10.11
#@ provenance: recorded
#@ driver: A user runs turbo run build --dry=json in a bun workspace with two packages, once on a clean tree and once after editing a file, adding an untracked file and staging another. turbo hashes the packages from git's view of the tree.
g init -q
act 'mkdir -p packages/a packages/b; printf "{\"name\":\"fx\",\"private\":true,\"packageManager\":\"bun@1.2.22\",\"workspaces\":[\"packages/*\"]}\n" > package.json; printf "{\"tasks\":{\"build\":{\"dependsOn\":[\"^build\"],\"outputs\":[\"dist/**\"]}}}\n" > turbo.json; printf "{\"name\":\"@fx/a\",\"version\":\"1.0.0\",\"scripts\":{\"build\":\"echo a\"}}\n" > packages/a/package.json; printf "{\"name\":\"@fx/b\",\"version\":\"1.0.0\",\"scripts\":{\"build\":\"echo b\"},\"dependencies\":{\"@fx/a\":\"workspace:*\"}}\n" > packages/b/package.json; printf "export const a = 1;\n" > packages/a/index.ts; printf "export const b = 2;\n" > packages/b/index.ts; printf "node_modules\n.turbo\n" > .gitignore'
act 'printf "{\n  \"lockfileVersion\": 1,\n  \"workspaces\": {\n    \"\": { \"name\": \"fx\" },\n    \"packages/a\": { \"name\": \"@fx/a\", \"version\": \"1.0.0\" },\n    \"packages/b\": { \"name\": \"@fx/b\", \"version\": \"1.0.0\", \"dependencies\": { \"@fx/a\": \"workspace:*\" } },\n  },\n  \"packages\": {\n    \"@fx/a\": [\"@fx/a@workspace:packages/a\"],\n    \"@fx/b\": [\"@fx/b@workspace:packages/b\"],\n  }\n}\n" > bun.lock'
g add -A
g commit -qm "chore: baseline"
tool turbo turbo run build --dry=json
act 'printf "export const a = 3;\n" > packages/a/index.ts; printf "export const c = 4;\n" > packages/b/extra.ts; printf "export const b = 5;\n" > packages/b/index.ts'
g add packages/b/index.ts
tool turbo turbo run build --dry=json
