#@ tool: turbo
#@ version: 2.10.11
#@ provenance: recorded
#@ driver: A user branches from master, edits one package and the lockfile, and asks turbo what changed with --affected, with a --filter on a git range and with turbo ls --affected. turbo diffs the branch against its base and reads the base lockfile with git show.
g init -q
act 'mkdir -p packages/a packages/b; printf "{\"name\":\"fx\",\"private\":true,\"packageManager\":\"bun@1.2.22\",\"workspaces\":[\"packages/*\"]}\n" > package.json; printf "{\"tasks\":{\"build\":{\"dependsOn\":[\"^build\"],\"outputs\":[\"dist/**\"]}}}\n" > turbo.json; printf "{\"name\":\"@fx/a\",\"version\":\"1.0.0\",\"scripts\":{\"build\":\"echo a\"}}\n" > packages/a/package.json; printf "{\"name\":\"@fx/b\",\"version\":\"1.0.0\",\"scripts\":{\"build\":\"echo b\"},\"dependencies\":{\"@fx/a\":\"workspace:*\"}}\n" > packages/b/package.json; printf "export const a = 1;\n" > packages/a/index.ts; printf "export const b = 2;\n" > packages/b/index.ts; printf "node_modules\n.turbo\n" > .gitignore'
act 'printf "{\n  \"lockfileVersion\": 1,\n  \"workspaces\": {\n    \"\": { \"name\": \"fx\" },\n    \"packages/a\": { \"name\": \"@fx/a\", \"version\": \"1.0.0\" },\n    \"packages/b\": { \"name\": \"@fx/b\", \"version\": \"1.0.0\", \"dependencies\": { \"@fx/a\": \"workspace:*\" } },\n  },\n  \"packages\": {\n    \"@fx/a\": [\"@fx/a@workspace:packages/a\"],\n    \"@fx/b\": [\"@fx/b@workspace:packages/b\"],\n  }\n}\n" > bun.lock'
g add -A
g commit -qm "chore: baseline"
g checkout -q -b topic
act 'printf "export const a = 3;
" > packages/a/index.ts; printf "
" >> bun.lock'
g commit -qam "feat: change a"
tool turbo env TURBO_SCM_BASE=master TURBO_SCM_HEAD=HEAD turbo run build --affected --dry=json
tool turbo turbo run build "--filter=[master...HEAD]" --dry=json
tool turbo env TURBO_SCM_BASE=master turbo ls --affected
