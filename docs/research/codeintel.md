# Agentic code intelligence in asc: search, symbols and vectors

Status: design for the checklist decision "agentic code intelligence built in" (`scion-checklist.md`, 2026-10-22). This file is not normative until its content is folded into SPEC.md and PLAN.md; §10 lists the edits those files need (not made here). Inputs: research of 2026-10-22 on code search engines (Blackbird, Zoekt, codesearch, livegrep, Cursor), symbol extraction (tree-sitter, stack graphs, SCIP, Sourcegraph) and embeddings for code (Cursor, Augment, Sourcegraph Cody, Continue, model cards, Rust crates). `docs/research/mcp.md` now exists and owns the MCP tool surface. This design adds no tool of its own: it implements mcp.md's `search` tool (unit M15, feature `search.v1`) and uses its `read_file` (§3.4).

Fixed rules this design obeys:
- no whole-repo scans, and indexing cost scales with change size;
- every index is keyed by content id (`FileId`) and deduplicated across revisions;
- read access holds inside every result list, ANN results included;
- the daemon's idle RSS stays ≤ 64 MiB, so heavy indexes live on the server (or a cache-tier replica), and the daemon serves only a bounded local subset;
- local-only mode works with a degraded local index;
- the embedding model runs locally by default, so no code leaves ASC infrastructure;
- `master`, not `main`. There is no IDE-specific surface: ASC's IDE plugins use the same RPC/IPC.

---

## 0. Answer to the user question: should asc have vector search for agents?

**Yes, but as the third of three tools. Exact search and symbols come first.** The point is that agents can query any revision, draft or stack **without a checkout**: all three indexes are server-side derived data that read blobs from the object store.

| Evidence | Number | Consequence for asc |
|---|---|---|
| Anthropic, Agent SDK guidance (2025-09) | "start with agentic search; add semantic search only if you need faster results or more variations" | grep/glob-style tools are primary; semantic is additive |
| Cursor, semantic search A/B (2025-11) | +12.5% Q&A accuracy (6.5–23.5%); +2.6% code retention on repos with > 1,000 files; grep-only had 2.2% more dissatisfied follow-ups | ASC has 4,543 files, above the threshold where vectors pay off |
| Zilliz claude-context (vendor) | −39% tokens, −36% tool calls, equal F1 | the gain is mostly efficiency: fewer tokens and turns |
| turbopuffer talk (vendor, 50 tasks) | file precision 65% → 87%; recall best with grep alone | agents need a **policy** for which tool to call (§3.4 tool descriptions) |
| Sourcegraph Cody, Continue | both dropped embeddings for search plus code graph; BM25F alone gave +20% at Sourcegraph | a good lexical ranker is the baseline; vectors must never be the only path |
| Cursor index sync (2026-01) | chunk embeddings cached by content hash; time to first query 7.87 s → 525 ms median | content addressing (our `FileId`) makes reuse free |

Decisions D1–D8:

| # | Decision |
|---|---|
| D1 | Three indexes, all derived data keyed by `FileId`: **grams** (doc-level trigram postings), **tags** (tree-sitter definitions and references), **chunks** (AST chunks plus embeddings). A revision is a set of `FileId`s, composed through `TreeId`-keyed doc sets (§1.3). |
| D2 | One engine for postings and BM25: **tantivy 0.27.0**. Segments, merges, FST term dictionary, bitpacked postings and mmap come from tantivy. asc writes only the trigram tokenizer, the regex-to-trigram planner and the scope filter. No custom posting store. |
| D3 | Cross-file navigation is **search-based**: reference names are joined to definition names, then ranked with import, project-graph and path signals. Stack graphs are archived and are not used. Compiler-grade SCIP is a v1.x upload path (I31). |
| D4 | The default embedding model is **jina-embeddings-v2-base-code** (Apache-2.0, 161M parameters, 768 dims, 8,192-token context). It runs on the CPU through **fastembed 7.2.0**. CodeRankEmbed (MIT, 137M) and Qwen3-Embedding-0.6B (Apache-2.0) are the bake-off candidates (I28). Changing the model is a config change plus a background re-embed. |
| D5 | ANN uses **usearch 2.26.5** HNSW over **i8** vectors on the server (4x smaller than f32, about 99% of quality with rescoring). Small scopes use exact brute force. The daemon's local index stores **b1** codes with i8 rescoring. |
| D6 | The ACL unit is the repository namespace (SPEC §15.1, §15.5). Each namespace has its own indexes, so no ANN graph ever contains another namespace's vectors. Within a namespace, every query is filtered by a scope bitmap **inside** the traversal. A path-level deny rule, if one is ever added, subtracts a subtree bitmap in the same predicate. |
| D7 | Indexing runs off the write path: an outbox effect enqueues a job, and workers drain the jobs. Readers report `coverage`. Exact search stays complete when the index lags, because the unindexed part of the scope is verified by streaming, within a budget. |
| D8 | Files that look like secrets are never embedded, never sent to an external provider, and are redacted in agent-facing snippets. Exact search still finds them, as `git grep` would for anyone with read access. |

---

## 1. Architecture

### 1.1 Per-content units (computed once per `FileId`, never again)

| Unit | Key | Content | Pure function of | Stored at |
|---|---|---|---|---|
| Doc row | `(ns, FileId)` → `ord: u32` | dense doc ordinal, size, flags that the bytes alone decide (`binary`, `invalid_utf8`, `too_large`, `long_line`, `too_many_grams`, `minified`, `secret`), per-index versions. No language, no attribute or config flag | file bytes only | SQL `ci_docs` |
| Doc languages | `(ns, ord, lang)` | one row per distinct language that the extension or shebang of any path holding the `FileId` gives (usually one; ≤ 4) | bytes plus path | SQL `ci_doc_langs` |
| Grams | doc `ord` in the `files` tantivy index | distinct ASCII-case-folded byte trigrams, as a multi-valued indexed `u64` field (24-bit value, doc-level, no positions) | bytes, `grams_ver` | tantivy segments (§1.4) |
| Tags | `d/tags.v1/<blake3(FileId ‖ lang ‖ tags_ver)>`, one object per `(FileId, lang)` | postcard `Vec<Tag { name, kind, role: Def\|Ref, range, name_range, container: u32, sig: Vec<u8>, doc: Vec<u8> }>` | bytes, grammar version, query version | object store (`d/`, evictable, recomputable) |
| Symbol terms | doc `ord` in `files` | `sym_def` and `sym_ref` raw terms; `ident` (camel/snake-split, lowercased, BM25 with frequencies); `doc` (comments and docstrings, BM25); `path_tok` (≤ 4 paths seen, ranking only); `lang` (multi-valued, from `ci_doc_langs`) | tags | tantivy segments |
| Chunk map | `d/chunks.v1/<blake3(FileId ‖ lang ‖ chunker_ver)>` | `Vec<(start_line, end_line, symbol, ChunkTextId)>` | tags plus bytes | object store |
| Chunk rows | `(ns, chunk_ord)` | `doc_ord`, `lang`, lines, `ChunkTextId = blake3::derive_key("asc v1 ci chunk", embed_text)` | chunk map | SQL `ci_chunks`; the `chunks` tantivy index (BM25 over `ident`, `doc`, `symbol`) |
| Vectors | `(ns, model_gen, ChunkTextId)` | i8 × dims | chunk text, model | vector segments (§1.4) |

- **Dedup:** a `FileId` gets one `ord` per namespace, however many paths and revisions hold it. An identical chunk text is embedded once per namespace and model generation (`ChunkTextId`). Embeddings are never shared across namespaces, so the rule of SPEC §3.3 against confirmation attacks also holds for indexes.
- **Byte skip rules** (doc flags; the doc gets no grams, tags or chunks): a NUL byte; invalid UTF-8; size > 1 MiB; any line > 2,000 bytes; > 20,000 distinct trigrams; the secret scan (§6). Only these are stored per `FileId`, because only the bytes decide them. These are the codesearch and Zoekt constants.
- **Path-dependent excludes are query-time, not stored per file:** `linguist-generated`, `linguist-vendored`, `-text`/`binary` attributes and the excludes of `.asc/codeintel.toml` depend on the path and the revision. The attribute files are read per directory with `.ascattributes` first and `.gitattributes` as the fallback (SPEC §2.4.1, checklist), never merged. The scope derivation (§1.3) computes the excluded bitmap `xdocs` per `(TreeId, rules digest)`, and every query uses `scope ∖ xdocs`. A file excluded at every path it has is indexed at backfill priority only; a query that sets `include_excluded` indexes it on demand. Indexing therefore never depends on ingest order, and the same `FileId` can be excluded at one path and included at another. On ASC they skip the 12 files holding 59% of the bytes (ONNX, WASM, jsonl corpora, `semantic-index.json`). Skipped and excluded files can still be found by path, and exact search verifies them by streaming when they are in scope and ≤ `core.bigFileThreshold`.
- **Embedding text** = language + enclosing symbol signature + the chunk body. The path is left out, so renames and copies keep their vectors. The path is a BM25 field and a ranking feature instead.

### 1.2 Chunking (for embeddings and chunk BM25)

The chunker splits the AST, then merges (cAST: +4.3 Recall@5 on RepoEval and +2.67 Pass@1 on SWE-bench against line chunking):
1. Walk the tree-sitter tree. A node of ≤ 1,536 non-whitespace characters (about 512 tokens) is a chunk candidate.
2. A larger class, impl or function node gets a **collapsed** chunk: its signature and member signatures with bodies replaced by `{ … }`, as Continue does. The walk then recurses into its children.
3. Adjacent small siblings are merged greedily up to the budget.
4. Files with no grammar get line windows of 60 lines with 10 lines of overlap.

`text-splitter` 0.33.0 (`code` feature) was considered and rejected: it does not do the collapsed-signature step. The chunker is about 300 lines over tags plus the tree.

### 1.3 Revision → set of `FileId`s (the scope), without walking the repo

| Piece | Definition | Cost |
|---|---|---|
| `tdocs.v1` (content-keyed derived value, like `mset.v1`) | key `TreeId`; value = roaring bitmap of the `ord`s of every File/Exec entry in the subtree, plus `dups`: a sorted list of `(ord, count)` for each ord that appears at ≥ 2 paths in the subtree (an ord that is not listed has count 1). Stored only for subtrees with ≥ 256 files. Smaller subtrees are expanded from their (cached) tree objects. | Derived from the children's `tdocs` plus the tree's own files. Unchanged subtrees are memo hits, so a new revision costs O(changed paths × depth) tree reads and bitmap unions. |
| `scope(rev)` | `tdocs(rev.root)` | one memo read once derived |
| Path filter | literal prefix `p` → `tdocs(lookup(rev.root, p))`; excludes subtract; globs are checked at locate time | O(depth) tree reads |
| Historical revision with no derived root | Probe the ancestors at the `skip.v1` distances (4, 16, 64) with one `has_many` for a derived `tdocs`. If one is found within 64 first-parent steps, `scope = tdocs(A.root) ⊕ Δ`, where Δ is built from the `changes` lists between them (metadata only, no new objects). A roaring set has no counts, so Δ is applied with counts: start from the `dups` of `tdocs(A.root)`, add 1 for each added path and subtract 1 for each removed path, and drop an ord from the bitmap only when its count reaches 0. Deleting one of two paths that hold the same `FileId` keeps the file. Otherwise derive `tdocs` from the tree, reusing every shared subtree. | O(log n) probes plus O(Δ), or O(changed spine) |
| `--stack` | scope = the top change's head (resolved through `ResolveChanges`) | as rev |
| `--stack-diff` | scope ∧ the ords of `paths(base..top)` (SPEC §4.5), with base = `merge_base(top, trunk)` via skip lists (§4.3) | O(log n + stack paths) |
| Workspace (daemon only) | server scope at `HEAD` minus dirty paths, plus the local overlay (§1.5) | O(dirty files) |
| Locate (ord → paths) | descend from the root only into subtrees whose `tdocs` contains the ord | O(depth) bitmap probes per hit |

- New `FileId`s met while deriving `tdocs` get an `ord` at once (one batched `INSERT … ON CONFLICT DO NOTHING`), and a file job is enqueued. An ord can exist before its content is indexed, so `coverage(scope, index) = |scope ∧ indexed[index]| / |scope|`.
- **Where `indexed[index]` lives:** a roaring bitmap per index, stored as an immutable bucket object `d/ciidx.v1/<blake3>` (≈ 50 KiB at 400k docs). `ci_manifest` names it (`indexed_ref`), and every manifest generation writes a new one in the same CAS. A query reads the bitmap of the generation it pinned, so coverage never overstates what that generation holds.
- The first `tdocs` of an imported head walks that head's tree once: O(files at the head), once per head ever, never per query. ASC: 4,543 files, 994 directories.
- `xdocs` is derived from the same walk: a directory whose rule digest is unchanged reuses its memo, so the cost stays O(changed spine).
- Sizes: ASC root bitmap ≈ 9 KiB. A HUGE root (400k files) ≈ 60 KiB, and a full spine ≈ 100 KiB per derived revision. Derivation is lazy (only revisions that are queried or warmed), and the values live in `d/` (evictable).

### 1.4 Where each piece lives, and budgets

| Piece | Authoritative copy | Hot copy | Size (estimate; I27 measures) |
|---|---|---|---|
| `ci_docs`, `ci_chunks`, `ci_jobs`, `ci_manifest` | server SQL | – | about 60 B per doc row and 40 B per chunk row; ASC ≈ 4 MiB |
| `files` and `chunks` tantivy indexes | immutable segment files in the bucket: `d/ciseg.v1/<blake3>`. `ci_manifest(ns, index, generation, segments, model_gen)` names the live set; a CAS on `generation` publishes a commit. | each replica's `disk_dir/ci/<ns>/`, mmapped; missing segments are downloaded on manifest change | grams ≈ 0.2x of indexed text; symbols and BM25 ≈ 0.1x. ASC HEAD text ≈ 45 MiB → ≈ 15 MiB. HUGE 25 GB → ≈ 7.5 GB |
| Tags, chunk maps, `tdocs` | `d/…` objects | server cache | tags ≈ 5–10% of source |
| Vector segments | bucket `d/civec.v1/<blake3>` (usearch index plus an ord map), named by `ci_manifest` | replica disk, opened with usearch `view` (mmap) | ≈ 0.9 KiB per chunk (768 B i8 + HNSW links, M = 16). ASC ≈ 30k chunks → ≈ 27 MiB. HUGE ≈ 3M chunks → ≈ 2.7 GB |
| Server memory | `[codeintel] memory = "2GiB"` is one budget with fixed shares: tantivy writer heaps (`workers × 32 MiB` for `files` plus one 32 MiB writer for `chunks`; about 288 MiB at 8 workers), the vector-merge builder (≤ 512 MiB, §4), doc-store caches (256 MiB), the query-embedding LRU (64 MiB), verify buffers (`limits.text_memory`, SPEC §16.4), and the rest as headroom. Startup fails when the shares exceed `memory`. | page cache does the rest | – |
| Daemon overlay | in memory, built on first query, dropped after 5 min idle | – | ≤ 16 MiB heap; ≤ 4,096 dirty files or ≤ 32 MiB text, otherwise streaming verify |
| Local index (local-only or offline) | `$ASC_HOME/ci/<repo-id>/` (tantivy plus b1/i8 vector files under `vec/<model_gen>/`, each with a header holding the model id and calibration digest) | the daemon mmaps it on query and closes the reader after 5 min idle | capped by `codeintel.local.maxSize` (default 512 MiB); LRU by revision |

- **Consistency:** a segment commit contains whole file jobs, and `indexed[index]` is read at the manifest generation a query pins. No query sees a half-indexed file, which gives Blackbird's commit-level consistency.
- **Compaction:** tantivy's log merge policy, run by the indexing leader. Superseded segments are deleted from the bucket 10 minutes after the generation that dropped them, so pinned page tokens stay valid.
- **One writer per namespace:** a SQL lease (`ci_manifest.leader`, `lease_until`). Any replica can index file jobs into new segments, and only the leader commits manifest generations and merges.
- **Forks:** `tdocs.v1`, `tags.v1`, `chunks.v1`, `ciseg.v1`, `civec.v1`, `ciidx.v1` and all `ci_*` rows are per-namespace derived data. `Fork` and `CopyClosure` (SPEC §3.3, §7) copy the reachable content closure only and never these objects, because ords differ per namespace and a copy would carry an index into a namespace whose readers may differ. A fork starts un-indexed and backfills its own heads (I15).
- **Cache-tier role:** `asc serve --role cache` serves no CodeIntel RPCs (SPEC §7: the tier RPCs only). A region that needs local query latency runs a read replica of `asc serve`, which syncs segments from the bucket.

### 1.5 Daemon and local-only

| Mode | What answers | Coverage |
|---|---|---|
| Online, workspace scope | The server answers at `HEAD`'s server revision, with `exclude_paths` = dirty paths. The daemon answers the same query over the overlay (dirty and staged files from the journal) and merges the two. | full |
| Online, any other scope | The server; the daemon only forwards | full |
| Offline, or local-only repo | The local index: grams, tags and BM25 for every `FileId` of the active workspaces' `HEAD` whose content is in `store/` or `cache/`. Built by the helper `asc codeintel worker` (a separate process, never the daemon), incrementally per local commit with the same `asc-codeintel` code. The helper is limited to: RSS ≤ 256 MiB (tantivy writer heap 64 MiB), 2 threads at low priority, disk only inside `codeintel.local.maxSize`, no network, no model. It is started only when the workspace is idle and exits after 60 s with no job. | degraded: `coverage` says how much; vectors only when `codeintel.local.embed = true` |
| Local semantic (opt-in) | The `asc-embed` helper binary, downloaded and minisign-verified by `asc codeintel enable-embed`, spawned on demand and exiting after 60 s idle. The daemon never loads a model, since a 161M-parameter model needs about 300–600 MiB. | as above |

---

## 2. Query semantics

| Query | Inputs (besides scope) | Plan | Output order |
|---|---|---|---|
| **search** literal / regex / word | `pattern`, `mode`, `case_sensitive`, `paths`, `exclude_paths`, `langs` | Port codesearch's `RegexpQuery` (exact/prefix/suffix/match sets, maxExact 7, maxSet 20, char class > 100 = any) to `regex-syntax` 0.8.11 HIR, using folded trigrams → tantivy `BooleanQuery` of `TermQuery` on `tri` → collector that keeps docs whose `ord` ∈ scope → verify each candidate with `regex` 1.13.1 over the bytes from `Objects::get` → locate. A query with no trigram (for example `.*`, or under 3 chars) is a streaming scan **of the scope**, allowed only with a path filter or a scope ≤ 2,000 files; otherwise `INVALID_ARGUMENT: pattern too broad`. | Ranked within a verified window of 2,000 files (features: symbol-definition hit, basename match, path depth, test or generated penalty), then by `ord`; cursor = last `(window, ord)` |
| **symbols def** | `name` (exact, or `prefix*`, or fuzzy with distance ≤ 1 via the term FST), `kinds` | `sym_def` term ∩ scope → load tags → filter | exact > same project (G07 project graph) > importer-near > path distance |
| **symbols refs** | `name`, optional `def` (path and line) | `sym_ref` term ∩ scope. Languages whose tags give no refs fall back to a word-boundary trigram search. When `def` is given, rank by import evidence (the ref file imports the def file's module or path) and project-graph reachability. | `confidence`: `import_match` > `name_match` |
| **symbols callers** | as refs | refs with role `Ref/call`, grouped by `container` (the enclosing definition) | by caller |
| **symbols outline** | `path` | tags of that one `FileId`; no index | source order |
| **semantic** | `query`, `mode = hybrid\|vector`, `k` | Embed the query (server, LRU keyed by `(model_gen, calibration digest, blake3(text))`, ≈ 20 ms on CPU). Choose the path by **selectivity** `s = kept chunks / namespace chunks` (kept = Σ chunk counts of the docs in `scope ∖ xdocs ∖ secret`, from `ci_docs`). If `s < 3%` or kept ≤ 20,000, run exact brute force over the kept vectors. Otherwise run usearch `filtered_search` with predicate `doc_ord(chunk) ∈ kept`, with `ef_search = min(4096, max(64, ceil(4·k / s)))` and a visit bound of `16 × ef_search` nodes. If the bound is hit with fewer than `k` results, fall back to brute force when kept ≤ 200,000 chunks, otherwise return the partial list with `ann_exhausted = true`. Each vector segment (§4) is searched on its own and the lists are merged. In `hybrid`, also run tantivy BM25 over the `chunks` index (`symbol` ×3, `ident` ×1, `doc` ×1, `path_tok` ×2) in the same scope and fuse with **RRF, k = 60**. Exact `sym_def` hits on query identifiers are a third list. Optional cross-encoder rerank (`bge-reranker-base` via fastembed), off by default. | fused score |

- **Every query takes a scope:** `rev` (any revspec, SPEC §4.2), `change` (a change id, resolved to its current head), `stack` (top change, with `diff_only`), or `workspace` (IPC only). The response echoes the resolved changeset and root `TreeId`, so agents can cite an immutable scope.
- **Pagination:** opaque `page_token` = postcard of `(manifest generation, scope digest, stage, last ord)`, MACed with the server key. An expired generation returns `FAILED_PRECONDITION: stale page token`.
- **Token-aware output:** the request carries `max_results`, `max_bytes` and `max_tokens`. Tokens are estimated as `ceil(bytes / 3)`, a conservative figure for code, with no tokenizer dependency. Results are grouped by file. A `FileId` at several paths is printed once with `also_at` (up to 5 paths). Long lines are cut at 300 bytes around the match. The response reports `est_tokens`, `truncated` and `next_page_token`.
- **Completeness under lag (D7):** exact search verifies the unindexed part of the scope by streaming, up to 2,000 files or 64 MiB, and sets `coverage = 1`. Above that it returns partial results with `coverage < 1` and enqueues the missing files at interactive priority. Symbols and semantic return `coverage` as it is.

---

## 3. Interfaces

### 3.1 RPC: `proto/asc/v1/codeintel.proto` (same port, `service CodeIntel`)

```proto
message Scope { RepoRef repo = 1; oneof at { string rev = 2; bytes change = 3; bytes stack_top = 4; bool workspace = 5; }
  bool stack_diff_only = 6; repeated string paths = 7; repeated string exclude_paths = 8; repeated string langs = 9; bool include_excluded = 10; }
message ResolvedScope { bytes changeset = 1; bytes root = 2; uint64 generation = 3; float coverage = 4; }
message Budget { uint32 max_results = 1; uint32 max_bytes = 2; uint32 max_tokens = 3; }
enum SearchMode { SEARCH_MODE_LITERAL = 0; SEARCH_MODE_REGEX = 1; SEARCH_MODE_WORD = 2; }
message SearchRequest { Scope scope = 1; string pattern = 2; SearchMode mode = 3; bool case_sensitive = 4;
  uint32 context_lines = 5; Budget budget = 6; string page_token = 7; }
message LineMatch { uint32 line = 1; repeated uint32 cols = 2; bytes text = 3; repeated bytes before = 4; repeated bytes after = 5; }
message FileHit { bytes path = 1; bytes file_id = 2; string lang = 3; repeated bytes also_at = 4; repeated LineMatch matches = 5; bool redacted = 6; }
message SearchResponse { ResolvedScope scope = 1; repeated FileHit files = 2; bool truncated = 3; string next_page_token = 4; uint32 est_tokens = 5; }
enum SymbolOp { SYMBOL_OP_DEF = 0; SYMBOL_OP_REFS = 1; SYMBOL_OP_CALLERS = 2; SYMBOL_OP_OUTLINE = 3; SYMBOL_OP_FIND = 4; }
message SymbolsRequest { Scope scope = 1; SymbolOp op = 2; string name = 3; bytes path = 4; repeated string kinds = 5;
  bytes def_path = 6; uint32 def_line = 7; Budget budget = 8; string page_token = 9; }
message Symbol { string name = 1; string kind = 2; bool is_def = 3; bytes path = 4; uint32 line = 5; uint32 end_line = 6;
  bytes signature = 7; string container = 8; bytes doc = 9; string confidence = 10; }
message SymbolsResponse { ResolvedScope scope = 1; repeated Symbol symbols = 2; bool truncated = 3; string next_page_token = 4; uint32 est_tokens = 5; }
enum SemanticMode { SEMANTIC_MODE_HYBRID = 0; SEMANTIC_MODE_VECTOR = 1; }
message SemanticRequest { Scope scope = 1; string query = 2; SemanticMode mode = 3; uint32 k = 4; Budget budget = 5; bool rerank = 6; }
message Chunk { bytes path = 1; bytes file_id = 2; uint32 start_line = 3; uint32 end_line = 4; string symbol = 5;
  double score = 6; repeated string sources = 7; bytes text = 8; bool redacted = 9; }
message SemanticResponse { ResolvedScope scope = 1; repeated Chunk chunks = 2; string model = 3; uint32 est_tokens = 4; bool ann_exhausted = 5; }
message IndexStatusRequest { Scope scope = 1; }
message IndexStatus { string index = 1; float coverage = 2; int64 lag_seconds = 3; uint64 generation = 4; string version = 5; }
message IndexStatusResponse { ResolvedScope scope = 1; repeated IndexStatus indexes = 2; }
service CodeIntel {
  rpc Search(SearchRequest) returns (stream SearchResponse);
  rpc Symbols(SymbolsRequest) returns (SymbolsResponse);
  rpc Semantic(SemanticRequest) returns (SemanticResponse);
  rpc IndexStatus(IndexStatusRequest) returns (IndexStatusResponse);
}
```

- **Rate class `codeintel`** (new, SPEC §16.3): `Search` and `Symbols` 50 req/s per principal (burst 100); `Semantic` 10 req/s per principal and 32 concurrent per replica (query embedding uses the CPU); `IndexStatus` is `cheap`. Search scans without trigrams count as `expensive`.
- **Permission:** read on every namespace searched. `Repo.Grep` (U47) stays as it is and is the streaming fallback that D7 reuses.
- **Workspace IPC** (`workspace.proto`, U73 amendment): the same four RPCs; `scope.workspace = true` is accepted only over IPC.

### 3.2 CLI (asc-only verbs; no git command changes behaviour)

| Command | Maps to |
|---|---|
| `asc search <pattern> [-F\|-E] [-i] [-w] [-C n] [--rev R\|--change C\|--stack [--diff-only]] [--lang L] [--json] [-- pathspec]` | `Search` (workspace scope by default inside a workspace, otherwise `master`) |
| `asc sym def\|refs\|callers <name> [--kind k] [--at path:line]`, `asc sym outline <path>`, `asc sym find <prefix>` | `Symbols` |
| `asc semantic "<query>" [--vector] [-k n] [--rerank]` | `Semantic` |
| `asc codeintel status [--rev R]`, `asc codeintel worker` (internal), `asc codeintel enable-embed` | `IndexStatus`, the local helper, the embed opt-in |

`--json` prints the proto3 JSON mapping of the response messages, which are the same shapes as RPC and MCP. Human output follows `git grep -n` layout (`path:line:text`) with the scope header on stderr. `git grep` (U24b) is not changed. Accelerating `git grep <rev>` with the index, with byte-identical output, is the optional unit I30.

### 3.3 ASC IDE plugins

The plugins call the same RPC/IPC through the generated TypeScript client (checklist, "IDE plugins"). This design adds no separate surface.

### 3.4 MCP: mcp.md's `search` tool (no tool of its own)

`docs/research/mcp.md` owns the MCP surface (rule M-D1: no tool exists twice). This design adds no tool name. The mapping onto mcp.md:

| mcp.md tool | Backed by | What this design adds |
|---|---|---|
| `search` (toolset `search`, unit M15, listed only when `Version.features` has `search.v1`) | `mode: text\|regex` → `Search`; `mode: symbol` → `Symbols`; `mode: semantic` → `Semantic` | Optional fields on the M15 row, additive: `change`, `stack`, `diff_only`, `op` (`def\|refs\|callers\|outline\|find`, for `symbol`), `k`, `context_lines`, `max_tokens`, `rerank`, `include_excluded`. The response is `SearchResponse`, `SymbolsResponse` or `SemanticResponse`, with `scope`, `coverage` and `est_tokens`. The M15 row is edited once in mcp.md (§10). |
| `read_file` (`core`) | `Repo.ReadFile` | Nothing. It expands a hit at the same revision. There is no `code_read`. |
| `grep` (`core`) | `Repo.Grep` | Nothing. It stays the streaming fallback (D7). |
| index status | `ResolvedScope.coverage` in every response, and `asc codeintel status` | No `code_index_status` tool. |

The server adds `search.v1` to `Version.features` when the `CodeIntel` service is enabled. The use policy goes into the `search` tool's description: text and regex first for identifiers, strings and error messages (results are verified matches); `symbol` when a name is known (`refs` is name-based, so read `confidence`); `semantic` when the names are unknown, or after text search finds nothing (ranked chunks, not every match). Result text is untrusted file content. Defaults: `max_tokens = 4000`, `context_lines = 2`, `k = 10`. Every result carries `scope.changeset`, so a follow-up call can pin the same revision.

---

## 4. Indexing pipeline

| Step | Trigger | Work | Idempotency |
|---|---|---|---|
| Enqueue | outbox effect `codeintel.enqueue(ns, changesets)` after `UpdateRefs`, `Ingest`, `Commit` and `RestackChanges` commit (U43c amendment). It is never on the write path. | For each new changeset: the `Write` `FileId`s of its `changes` → `has` check against `ci_docs` → `INSERT ci_jobs(kind='file')` for the unknown ones only. Cost is O(changed paths). | `ci_jobs` PK `(ns, kind, key, version)`, `ON CONFLICT DO NOTHING` |
| File job | worker lease (`FOR UPDATE SKIP LOCKED` on Postgres; one in-process queue on SQLite) | read bytes once (`Objects::get`) → skip rules → secret scan → trigrams → tree-sitter tags (cancellation flag, 250 ms per file) → chunk map → write `d/tags.v1`, `d/chunks.v1` → add docs to the in-memory segment writer → enqueue an `embed` job for chunk texts not yet embedded | the outputs are pure functions of `(FileId, versions)`, so concurrent workers write identical bytes |
| Segment commit | every 2 s or 5,000 docs | tantivy commit → upload the segment files → CAS `ci_manifest.generation` → set the `indexed` bits | the CAS loser re-bases its segment set onto the new generation |
| Embed job | batches of 32 chunk texts | fastembed → i8 quantisation (per-dimension min/max, calibrated once per model on 10k ASC chunks; the table is stored in the model record with its blake3 digest) → add to the vector segment writer | key `(ns, model_gen, ChunkTextId)` |
| Merge | leader, background | tantivy merge policy. Vector segments use **tiered merges**: only segments of similar size merge, 4 at a time, and no output exceeds 512 MiB (≈ 570k chunks). The merge reads its inputs through mmap and adds their vectors one by one to a new index, so peak memory is the output segment (≤ 512 MiB, the share in §1.4) and never the whole namespace. HUGE (≈ 2.7 GB) settles at 5–6 segments, which queries search separately and merge. One merge runs at a time per namespace. | new generation; old segments deleted 10 min later |
| Sweep | leader, weekly | Live ords = union of `tdocs` of the warmed heads (land-branch tips, open PR heads, drafts touched in the last 30 days). An ord that has been dead for 14 days is tombstoned: deleted from the `files` and `chunks` tantivy indexes by ord, dropped from vector segments at their next merge, and its `d/tags.v1` and `d/chunks.v1` objects removed. Ords are never reused. Content that becomes reachable again is re-enqueued as new work. | idempotent; a new generation |
| Scope warm | follows `ref_log`, like `projects_warm` (SPEC §12A.5) | derive `tdocs` for new land-branch tips, `refs/pull/*/head` and draft change heads | memo |

- **Priorities:** interactive (a query met unindexed docs) > land-branch tips > PR and draft heads > backfill. Worker concurrency is `[codeintel] workers` (default: cores / 2), and workers run at lower priority (`nice` 10).
- **Backfill on import (monorepo):** index only what is needed. U33/U45 import ends with `codeintel.enqueue` for the tips of `master`, the release branches and the open PR heads, not for every historical commit. ASC: about 4.5k `FileId`s at `master` and a few hundred more for PR heads, against 49,966 blobs ever. Older revisions are indexed on demand the first time someone queries them (D7 keeps exact search complete while that happens). `asc serve admin codeintel backfill --refs <glob> [--since <date>]` exists for an explicit wider backfill.
- **Model changes:** the model id is `name@<sha256 of the ONNX file>/<quantisation>/calib@<calibration digest>/<chunker_ver>`. A change to any part, including a new calibration, creates `model_gen + 1`. The reranker is pinned by sha256 in the model registry and is part of the `model` string that `SemanticResponse` echoes; it holds no vectors, so changing it needs no re-embed. The query-embedding LRU key includes `model_gen` and the calibration digest. Embed jobs for the chunks reachable from warmed heads (not history) are enqueued at backfill priority. Queries keep using the old generation until the new one reaches coverage ≥ 0.99 on every land-branch tip, then `ci_manifest.model_gen` flips atomically. The old vectors are deleted after 7 days. Vectors from two generations are never compared, and the query is always embedded with the generation it searches. **Local vectors (I20)** carry the model id and calibration digest in their file header and live under `vec/<model_gen>/`. When the server's `model_gen` (online) or the pinned local model id (offline) differs, the helper drops them and rebuilds them for the active workspace `HEAD`s; they are never migrated.
- **Extractor versions** (`grams_ver`, `tags_ver`, `chunker_ver`) bump the same way: lazy re-extraction for warmed heads only.
- **CPU path (default):** int8-quantised ONNX via ort, batch 32, threads = workers. Estimate: about 40 chunks/s per core for chunks of ≤ 512 tokens, to be measured by I27. ASC HEAD (≈ 30k chunks) on 16 cores ≈ 50 s, once. A median commit (4 files, ≈ 24 chunks) takes < 1 s.
- **GPU (optional):** `[codeintel.embed] device = "cuda"` (ort CUDA EP), or run `asc serve --role indexer` on a GPU host. The indexer role runs only workers, with no RPC serving. The CPU path is always valid; the GPU only changes throughput.
- **External providers:** opt-in only in `asc-serve.toml` (`provider = "openai-compatible"`, `url`, `allow_external = true`). `.asc/codeintel.toml` in a repo can never enable one. Secret-flagged chunks are never sent.

---

## 5. Component choices

| Need | Choice (version) | Rejected | Why, with numbers |
|---|---|---|---|
| Postings, segments, BM25 | **tantivy 0.27.0** (MIT, MSRV 1.88 ≤ our 1.95): indexed multi-valued `u64` trigram field (no positions) plus text fields | custom posting store; Zoekt-style positional trigrams; livegrep suffix arrays | Doc-level grams cost ≈ 0.2x of source (codesearch: Linux 420 MB → 77 MB); positional grams cost 3.5x (Zoekt) and suffix arrays 3–5x, and suffix arrays are hard to update incrementally. tantivy gives segment/merge/delete, an FST term dictionary and Lucene-equal BM25 for free. Its `RegexQuery` matches whole terms only, so the planner is ours. |
| Regex planner | port of codesearch `index/regexp.go` (≈ 1k LoC) onto **regex-syntax 0.8.11** HIR; verification with **regex 1.13.1** (workspace) | `lera-trigram`, `ix`, `grix` (low adoption) | No Rust crate is widely used. The algorithm is small and is property-tested against brute force. |
| Gram scheme | ASCII-case-folded **trigrams** v1 | sparse grams (Cursor, GitHub, ClickHouse) | Simplest correct choice. Sparse grams cut false positives (13-char `MAX_FILE_SIZE`: 17 grams at index time, a minimal covering set at query time) but need a bigram weight table. They become `grams.v2` only if I27 shows a candidates/matches ratio > 4 on HUGE. |
| Bitmaps | `roaring` (MIT/Apache; pin the current 0.10.x at I05) | sorted `Vec<u32>` | scope ∧ postings ∧ ACL for 400k docs in microseconds; ≈ 60 KiB HUGE root |
| Parsing and tags | **tree-sitter 0.27.1** and **tree-sitter-tags 0.27.1** (MSRV 1.90; both versions exist on crates.io); grammars: tree-sitter-rust 0.24.2, tree-sitter-typescript 0.23.2 (TS and TSX), tree-sitter-python 0.25.0, tree-sitter-c-sharp 0.23.5, tree-sitter-go 0.25.0, plus tree-sitter-javascript (pin at I03). All load through `tree-sitter-language` 0.1.9, within ABI 13–15. | stack-graphs 0.14.1 (archived 2025-09, pinned to tree-sitter 0.24, 4 languages); ast-grep as a resolver (no scopes or types) | Tags are a pure function of the file bytes, which suits caching by `FileId`. Parse plus query takes milliseconds per file. **Project-owned `tags.scm`** for all six languages, because upstream is thin (C# has about 12 patterns and no fields, properties or records; TS has 7 TS-specific patterns). A reported break of typescript 0.23.2 against core ≥ 0.25 is unconfirmed, so I03 tests it first and vendors the grammar if it fails. ASC mix: ts 2,616, tsx 84, rs 188, py 152; `surql` (287) gets grams only. |
| Embedding runtime | **fastembed 7.2.0** (Apache-2.0, `default-features = false`, so no `hf-hub` download) over **ort =2.0.0-rc.15** (ONNX Runtime 1.31; pinned exactly, because it is an RC; checked on crates.io: fastembed 7.2.0 requires `=2.0.0-rc.15`). Models load through `UserDefinedEmbeddingModel` from bytes that `asc-embed` has verified against the registry sha256 and minisign (§6); fastembed never fetches. fastembed's own `tokenizers` dependency enables `onig` (C code). I09 decides between keeping it (MSVC CI job, C entry in the allow-list) and calling `ort` and `tokenizers` with `fancy-regex` directly. candle-core 0.11.0 only if a candle-only model wins I28 | Python sidecar; text-embeddings-inference | in-process Rust, ships rerankers too, DirectML/CUDA EPs. The API churns between majors, so pin. |
| Default model | **jina-embeddings-v2-base-code** (Apache-2.0, 161M, 768-d, 8k ctx, built into fastembed) | jina-code-embeddings 0.5B/1.5B (**CC-BY-NC**); nomic-embed-code (7B, GPU-class; it becomes an optional GPU-tier model) | Zero-effort, licence-clean and CPU-sized. **Bake-off (I28)** against CodeRankEmbed (MIT, 137M, CoIR 60.1, CSN MRR 77.9) and Qwen3-Embedding-0.6B (Apache-2.0, MRL 32–1024 dims). A candidate wins if it beats the default by ≥ 3 NDCG@10 on the ASC eval set within 2x embed time. Model licences are checked against an allow-list (MIT, Apache-2.0) in `asc-embed`, as cargo-deny does for crates. |
| ANN | **usearch 2.26.5** (Apache-2.0; C++ FFI, so it needs the §15.2 unsafe/FFI allow-list entry for the vector module and an MSVC CI job, and sits behind the cargo feature `ann`, off in `asc.exe`): HNSW, i8 cosine, M = 16, ef_construction 128, `ef_search` by the formula of §2; `filtered_search` predicate; `view` mmap | hnsw_rs 0.3.5 (graph cannot be mmapped); arroy 0.9.0 (LMDB, Annoy trees); lancedb 0.40.0 (pulls in Arrow and DataFusion) | Filtering inside the traversal is how D6 holds within ANN. mmap keeps RSS as page cache. Native i8/b1 support. One immutable file per segment fits the bucket-segment model. |
| Quantisation | server **i8** (4x smaller, ≈ 97% raw and ≈ 99% with rescoring); daemon **b1** (32x, ≈ 92.5% raw and ≈ 96% with i8 rescoring of the top 200) | f32 everywhere | Augment: bit vectors plus rescoring gave 8x less memory, latency from > 2 s to < 200 ms and 99.9% parity at 100M LoC. The b1 loss depends on the model (e5-base-v2 kept only 75%), so I28 measures b1 retention for the chosen model, and the daemon uses i8-only if retention is < 93%. |
| Fusion | **RRF, k = 60** (Cormack et al. 2009), about 10 lines | learned fusion; score normalisation | model-free; equal to or better than CombMNZ and Condorcet Fuse on TREC; LanceDB and Elasticsearch use it by default |

`regex` 1.13.1 and `regex-syntax` 0.8.11 match `Cargo.lock`. All crates pass `cargo deny` as configured (MIT, Apache-2.0, Unlicense and others). Zoekt and ripgrep are **bench-only tools**, not dependencies.

---

## 6. Security

| Threat | Control |
|---|---|
| Reading another repository's code through search | Indexes are per namespace (D6). The server authorises read on each namespace **before** opening its indexes. Composed repositories (gitlinks, SPEC §10.6) are searched only when the caller can read the child namespace; otherwise the gitlink subtree is not in scope and its results are absent, not redacted. Results from several namespaces are merged after each namespace has been filtered on its own. |
| Leakage inside ANN results | The HNSW graph of a namespace holds only that namespace's vectors. `filtered_search` applies `scope ∧ ¬secret ∧ ¬deny` during traversal, so out-of-scope chunks never take a result slot. Brute force for small scopes uses the same predicate. Test (I16): a deny subtree never appears at any `k`, even with an exact-duplicate query. |
| Path-level read rules (if SPEC ever adds them) | They subtract the denied subtree's `tdocs` in the same predicate. Locate skips denied subtrees, so a `FileId` that is also at an allowed path is shown only at the allowed path. |
| Existence probing through timing or dedup | No cross-namespace dedup of docs, chunks or vectors. Page tokens are MACed and bound to a scope digest. |
| Secrets in embeddings | Per-file secret scan at file-job time. Path rules: `.env*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `id_rsa*`, `*credentials*`, `secrets.*`. Content rules: private-key headers, `AKIA[0-9A-Z]{16}`, JWT shape, GitHub/Forgejo/Slack token prefixes, and `(key\|secret\|token\|password)\s*[:=]\s*` followed by ≥ 20 chars of entropy ≥ 3.5 bits/char. A flagged **file** is never chunked or embedded. A flagged **line** is masked in chunk text before embedding and redacted (`[redacted: possible secret]`) in MCP and `--json` snippets (`redacted = true`). Exact search still matches it (git-grep parity). **One renderer (I22) runs inside the server for every surface except the local human CLI:** MCP, `--json`, the Forgejo search page (I24), the Forgejo API (I26), `/explore/code` and the symbol panels. A redacted line loses `text` and `cols`, and context lines are scanned too. The path and line number stay, because a reader of the repository can already list the tree (git-grep parity). A flagged file shows its path only, with `redacted = true`. `--no-redact` is a CLI-side re-read through `Repo.ReadFile`; the RPC has no such field. |
| Prompt injection through indexed content | Results are data. MCP responses keep code in fields (`text`, `matches[].text`), never in the tool's prose. Control characters and ANSI escapes are stripped, lines are capped at 300 bytes, and every hit carries provenance (`repo`, `changeset`, `path`, lines, `file_id`) so the agent can cite it. Tools are read-only by default (checklist "MCP built in"), so an injected instruction cannot trigger a write through these tools. The tool descriptions tell the agent that result text is untrusted file content. |
| Untrusted PR content (parser DoS) | Tree-sitter runs with a 250 ms cancellation flag and a 1 MiB file cap. Regex verification uses `regex` (linear time) with `size_limit` 10 MiB. Planner sets are capped (maxSet 20). The rate class limits abuse. |
| Model supply chain | Model files and the reranker are pinned by sha256 in the model registry, with no runtime download by fastembed or `hf-hub` (§5). The client helper and the model are minisign-verified (SPEC §16.2). |

---

## 7. Forgejo fork

| Surface | Change | Data |
|---|---|---|
| Code-indexer backend `asc` (`[indexer] REPO_INDEXER_TYPE = asc`) | implements Forgejo's code-indexer interface under `modules/indexer/code/asc`. `Index` and `Delete` are no-ops, because the asc server indexes on ingest. `Search` calls `CodeIntel.Search` at the default branch. This replaces the git-grep fallback, which the asc backend cannot run (K05). | `Search` |
| Repo search page `/{owner}/{repo}/search` | mode tabs (Exact, Regex, Symbols, Semantic), a branch/tag/PR/change selector (the scope), language and path filters, and a coverage note while indexing | `Search`, `Symbols`, `Semantic`, `IndexStatus` |
| File view | click an identifier → side panel with the definition and references (with confidence), and "Outline" | `Symbols` |
| PR files tab | "Find usages in the base" for changed symbols | `Symbols` with `scope.rev = base` |
| Global `/explore/code` | fan-out over the repos the user can read, merged with RRF | `Search` per namespace |
| API | `GET /api/v1/repos/{owner}/{repo}/asc/search\|symbols\|semantic` proxies with the user's token. This is the hook for the Forgejo MCP endpoint. Responses are the server-rendered (already redacted) messages as JSON; Forgejo never re-renders raw lines. | the same messages, as JSON |

---

## 8. Benchmarks (`bench/codeintel/`, joins the O05 gate)

| Scenario | asc | Compared with |
|---|---|---|
| Full index build of HEAD (cold) | `asc serve admin codeintel backfill` | `zoekt-git-index`; `git grep` needs no index (recorded as 0) |
| Incremental index for a commit | ASC's commit sizes replayed (median 4 files, p95 35) | Zoekt delta build |
| Literal rare / literal common (`for`) / regex `\bfoo\w+Bar\b` / case-insensitive / `-w` | `asc search` at `master`, no checkout | `rg` on a warm checkout; `git grep` (worktree); `git grep <rev>` (no checkout); `zoekt` |
| Same query, scope = a draft stack | `asc search --stack` | `git grep <sha>` |
| `def` / `refs` / `callers` / `outline` | `asc sym …` | `rg -w` (name match) as the baseline |
| Semantic and hybrid | `asc semantic` | BM25-only (`chunks` index) as the baseline |

**Workloads:** small (ASC), medium (linux kernel import), HUGE (`bench/gen` for grams and symbols; an optional chromium snapshot for semantic, because synthetic text is not real code). **Metrics:** index build time per GB, index bytes per GB of source, incremental latency p50/p99, query p50/p99 (server time and end-to-end), candidates/matches ratio, server RSS, daemon RSS idle and under load, tokens returned per query, and semantic NDCG@10 and Recall@10 on the eval set (I28: about 200 ASC queries labelled automatically from history, with the PR title as the query and the changed files as relevant).

| Gate | Value |
|---|---|
| Exact search, HUGE, server p50 / p99 | ≤ 50 ms / ≤ 300 ms (the Zoekt design target is < 50 ms) |
| Exact search vs `git grep <rev>` and cold `rg` | ≥ 10x faster on medium and HUGE; not slower on small |
| Incremental index of a median ASC commit | grams and tags ≤ 2 s; with embeddings on CPU ≤ 10 s |
| Index size | grams plus symbols ≤ 0.4x of indexed text |
| Daemon | idle RSS ≤ 64 MiB with the overlay and the local reader closed; ≤ 96 MiB while serving |
| Semantic | hybrid NDCG@10 ≥ BM25-only + 5 points on the ASC eval set; otherwise `search` with `mode = semantic` defaults to BM25 mode, and the result is recorded in `docs/BENCHMARKS.md` |

---

## 9. Proposed PLAN units (I01–I31)

New crates: `asc-codeintel` (extraction, planner, tantivy schema, scope algebra, ranking, rendering; depends on `asc-engine`) and `asc-embed` (fastembed, the model registry and the helper binary; built only for the server and the opt-in helper, so `asc.exe` does not ship ONNX Runtime). Each unit owns its test files. M15 and M08 are units of `docs/research/mcp.md`.

**Phases:** 4c after U47 (I01–I17, I22, I29); 4c-MCP after M15 (I21); 5c after U53b/U73 (I18–I20); 9c on the Go track after K10 (I23–I26); O-track (I27, I28); v1.x (I30, I31).

**Recorded amendments:** U43c outbox gains the `codeintel.enqueue` effect (I13); U73 `workspace.proto` gains the four CodeIntel RPCs (I18); U33/U45 import completion enqueues heads (I15); U69 gains the `codeintel` rate class (I16).

| ID | Title | Crate / repo | Depends | Owns | Accept |
|---|---|---|---|---|---|
| I01 | Doc model: ords, language set per path, byte skip rules, `.asc/codeintel.toml`, secret scanner | asc-codeintel | U04, U01b, U24b | `crates/asc-codeintel/src/{lib.rs,doc.rs,lang.rs,skip.rs,config.rs,secrets.rs}` | `cargo test -p asc-codeintel -- doc:: skip:: secrets::` |
| I02 | Trigram extraction and regex-to-trigram planner (codesearch port) | asc-codeintel | I01 | `crates/asc-codeintel/src/{grams.rs,plan.rs}`, `tests/plan_prop.rs` | `cargo test -p asc-codeintel --test plan_prop` (proptest: the plan never excludes a matching doc) |
| I03 | Tree-sitter tags: six grammars, project-owned `tags.scm`, containers, outline | asc-codeintel | I01 | `crates/asc-codeintel/src/tags/**`, `crates/asc-codeintel/queries/*/tags.scm`, `tests/tags_golden.rs` | `cargo test -p asc-codeintel --test tags_golden` (includes the TS 0.23.2 vs core 0.27.1 smoke test) |
| I04 | AST chunker (split, merge, collapsed signatures) | asc-codeintel | I03 | `crates/asc-codeintel/src/chunk.rs` | `cargo test -p asc-codeintel chunk::` |
| I05 | Scope algebra: `tdocs.v1` with `dups` counts, `xdocs` (attributes with `.ascattributes` first, config excludes), path filters, counted skip-list delta, stack-diff, locate | asc-codeintel | I01, U12, U14, U14b, U24b | `crates/asc-codeintel/src/scope.rs`, `tests/scope_oracle.rs` | `cargo test -p asc-codeintel --test scope_oracle` (equals a full-walk oracle, including a duplicate-path deletion; changeset reads bounded by O(changed spine)) |
| I06 | tantivy schema (`files`, `chunks`), segment writer and reader, bucket-persisted segments, manifest CAS | asc-codeintel | I02, I03, U07 | `crates/asc-codeintel/src/index/{schema.rs,segments.rs,manifest.rs}` | `cargo test -p asc-codeintel index::` |
| I07 | Exact search executor: plan → postings ∧ scope → verify → locate; streaming completion under lag | asc-codeintel | I05, I06, U47 | `crates/asc-codeintel/src/search.rs` | `cargo test -p asc-codeintel search::` (results equal `git grep <rev>` on fixtures) |
| I08 | Symbol queries: def, refs, callers, outline, find; import and project-graph ranking | asc-codeintel | I05, I06, G07 | `crates/asc-codeintel/src/symbols.rs` | `cargo test -p asc-codeintel symbols::` |
| I09 | `asc-embed`: fastembed runtime, model registry (sha256, licence allow-list), i8 calibration, helper binary protocol | asc-embed | U01b | `crates/asc-embed/**` | `cargo test -p asc-embed` (a tiny test model; `--ignored` runs the real model) |
| I10 | Vector segments: usearch i8 HNSW, filtered search, brute-force small scopes, b1 local format, model generations | asc-codeintel | I05, I09 | `crates/asc-codeintel/src/vectors.rs`, `tests/ann_acl.rs` | `cargo test -p asc-codeintel --test ann_acl` (no out-of-scope chunk at any k) |
| I11 | Hybrid ranking: chunk BM25 + vector + symbol list, RRF k = 60, optional reranker | asc-codeintel | I08, I10 | `crates/asc-codeintel/src/hybrid.rs` | `cargo test -p asc-codeintel hybrid::` |
| I12 | `codeintel.proto` and conversions | asc-rpc | U41 | `proto/asc/v1/codeintel.proto`, `crates/asc-rpc/src/codeintel.rs` | `buf lint proto && cargo test -p asc-rpc codeintel::` |
| I13 | Job queue `ci_jobs`, outbox trigger, leases, priorities | asc-serve | U43c, I06 | `crates/asc-serve/src/codeintel/queue.rs`, `crates/asc-serve/migrations/{postgres,sqlite}/0301_codeintel.sql`; amendment: U43c outbox effect | `cargo test -p asc-serve codeintel::queue` (enqueue is O(changed paths); duplicates are no-ops) |
| I14 | Workers: file, embed, tiered merge, sweep and scope-warm jobs; model generation flip | asc-serve | I13, I04, I10 | `crates/asc-serve/src/codeintel/{worker.rs,warm.rs,modelgen.rs,sweep.rs}` | `cargo test -p asc-serve -- codeintel::worker codeintel::modelgen codeintel::sweep` |
| I15 | Import backfill (heads only), `admin codeintel backfill`, on-demand indexing | asc-serve | I14, U33, U45; amendment to U33/U45 | `crates/asc-serve/src/codeintel/backfill.rs`, `crates/asc-serve/tests/codeintel_fork.rs` | `cargo test -p asc-serve -- codeintel::backfill codeintel_fork` (only head `FileId`s are enqueued; `Fork` and `CopyClosure` copy no `tdocs` or `ci*` objects or rows) |
| I16 | `CodeIntel` RPC handlers: authz per namespace, composed fan-out, page tokens, rate class | asc-serve | I07, I08, I11, I12, I22, U44, U60, U69, C19 | `crates/asc-serve/src/codeintel/rpc.rs`, `tests/codeintel_acl.rs`; amendment: U69 class | `cargo test -p asc-serve --test codeintel_acl` |
| I17 | CLI `asc search`, `asc sym`, `asc semantic`, `asc codeintel status`, with `--json` (needs U50) | asc | I16, U50 | `crates/asc/src/cli/codeintel/**`, `crates/asc/tests/codeintel_cli.rs` | `cargo test -p asc --test codeintel_cli` |
| I18 | Daemon overlay index and workspace-scope merge; Workspace IPC methods | asc | I07, U53b, U73 | `crates/asc/src/daemon/codeintel_overlay.rs`, `crates/asc/tests/codeintel_overlay.rs`; amendment: U73 `workspace.proto` | `cargo test -p asc --test codeintel_overlay` (idle RSS ≤ 64 MiB after the reader closes) |
| I19 | Local degraded index: the `asc codeintel worker` helper (limits of §1.5), `$ASC_HOME/ci`, size cap, offline routing | asc | I06, I08, I18 | `crates/asc/src/codeintel_local/**`, `crates/asc/tests/codeintel_local.rs` | `cargo test -p asc --test codeintel_local` (works with `--store` local-only; reports coverage) |
| I20 | Local embedding opt-in: helper download and verify, b1/i8 local vectors | asc, asc-embed | I19, I10 | `crates/asc/src/codeintel_local/embed.rs` | `cargo test -p asc codeintel_local::embed` |
| I21 | Wire mcp.md's `search` tool (M15) to `CodeIntel`: mode mapping, additive fields, tool description, `search.v1` in `Version.features` | asc-mcp, asc-serve | I16, M15, M08 | `crates/asc-mcp/tests/search_codeintel.rs`, `crates/asc-serve/src/codeintel/features.rs`; amendment: M15 row | `cargo test -p asc-mcp --test search_codeintel && cargo test -p asc-serve codeintel::features` |
| I22 | Output renderer: token budget, `also_at` dedup, redaction, control-char stripping, provenance | asc-codeintel | I01, I07 | `crates/asc-codeintel/src/render.rs` | `cargo test -p asc-codeintel render::` (every non-CLI surface uses this renderer) |
| I23 | Forgejo `asc` code-indexer backend | forgejo | K06, K10, I16 | `modules/indexer/code/asc/**` | `go test ./modules/indexer/code/asc/...` |
| I24 | Repo search page: modes, scope selector, coverage note; shows server-redacted text only | forgejo | I23 | `routers/web/repo/asc_search.go`, `templates/repo/asc_search.tmpl` | `go test ./routers/web/repo/... -run TestAscSearch` (an AWS-key fixture line is absent from the HTML) |
| I25 | File-view symbol panel and outline; PR "find usages in base" | forgejo | I23 | `routers/web/repo/asc_symbols.go`, `templates/repo/asc_symbols.tmpl` | `go test ./routers/web/repo/... -run TestAscSymbols` |
| I26 | `/explore/code` fan-out and the `/api/v1/.../asc/{search,symbols,semantic}` API | forgejo | I23 | `routers/web/explore/asc_code.go`, `routers/api/v1/repo/asc_codeintel.go` | `go test ./routers/... -run 'TestAscExploreCode|TestAscCodeintelAPI'` (an AWS-key fixture line is absent from the JSON) |
| I27 | `bench/codeintel`: scenarios against rg, git grep and Zoekt; gates of §8 | bench | O05, I17 | `bench/codeintel/**` | `cargo run -p asc-bench --release -- --suite codeintel-quick` |
| I28 | Semantic eval set from ASC history; model and quantisation bake-off | bench | I27, I14 | `bench/codeintel/eval/**` | `cargo run -p asc-bench --release -- --suite codeintel-eval` |
| I29 | Metrics `asc_ci_index_lag_seconds`, `asc_ci_jobs{kind,state}`, `asc_ci_query_seconds{rpc}`, `asc_ci_candidates_ratio`, `asc_ci_coverage`; runbook | asc-serve, docs | I13, O06 | `crates/asc-serve/src/codeintel/metrics.rs`, `docs/runbooks/tier-codeintel.md` | `cargo test -p asc-serve codeintel::metrics` |
| I30 | (v1.x) Index-accelerated `git grep <rev>` with byte-identical output | asc | I07, U24b | `crates/asc/src/cli/grep_index.rs` | `cargo test -p asc-compat --test compat -- grep` |
| I31 | (v1.x) SCIP upload tier: `asc codeintel upload-scip`, documents keyed by `FileId`, precise def/refs where present | asc-serve, asc | I08, I16 | `crates/asc-serve/src/codeintel/scip.rs`, `crates/asc/src/cli/codeintel/scip.rs` | `cargo test -p asc-serve codeintel::scip` |

Required tests (beyond each acceptance command):
- **I02:** `/Google.*Search/` plans to the 8-trigram AND; case-folded plans; a class wider than 100 chars becomes match-all.
- **I05:** a new revision derives at most `depth × changed paths` new `tdocs` objects; for a historical revision within 64 steps, scope = checkpoint ⊕ Δ with no tree reads. `tests/scope_oracle.rs` has a duplicate-path case: two paths hold one `FileId`, one is deleted and the file stays in scope; deleting both removes it. A file excluded by attributes at one path and not at another is in scope only at the second, and `.ascattributes` wins over `.gitattributes` in the same directory.
- **I06/I10:** a merge of 20 vector segments stays within the 512 MiB merge share, and the writer heaps plus the merge fit `[codeintel] memory`.
- **I10:** a filter that keeps 1% of a 3M-chunk fixture takes the brute-force path; a 30% filter takes HNSW and never exceeds the visit bound.
- **I10/I16:** an ACL deny and a non-readable gitlink child are absent from `Semantic` at k = 1…1000; namespace A's queries never open namespace B's segments.
- **I14:** a model change (including a new calibration) keeps serving the old generation until coverage ≥ 0.99, then flips once; the query LRU never returns an embedding from another `model_gen`; a sweep removes the ords of an abandoned draft after the grace period and keeps those reachable from an open PR head.
- **I22:** a fixture file with an AWS key is never embedded, and its line is redacted in MCP output.

---

## 10. Edits needed in SPEC.md and PLAN.md (not made here)

- **§0 / §1.1:** reference the "agentic code intelligence" decision and this design.
- **§5:** add the content-keyed type `tdocs.v1` (key `TreeId`) and the objects `tags.v1`, `chunks.v1`, `ciseg.v1` and `civec.v1` under `d/`.
- **§7:** add `service CodeIntel` (`codeintel.proto`), and the `workspace.proto` methods. **§8.2:** add the tables `ci_docs`, `ci_chunks`, `ci_jobs` and `ci_manifest`.
- **§8.1:** add the `[codeintel]` config (`memory`, `workers`, `embed.{model,device,provider,allow_external}`) and `--role indexer`.
- **§9.1:** the daemon serves CodeIntel IPC with the overlay and closes local readers when idle; it never loads a model.
- **§11.2:** add the asc-only verbs `search`, `sym`, `semantic` and `codeintel`.
- **§15:** add the §6 controls, including "indexes are per namespace" and the secret-redaction rule.
- **§16.1 / §16.3 / §16.4:** add the metrics, the `codeintel` rate class and the §8 gates.
- **§13.3:** add I23–I26 to the Forgejo patch stack after K10.
- **§3.3 / §7:** state that `Fork` and `CopyClosure` copy the reachable content closure only, never `d/tdocs.v1`, `d/tags.v1`, `d/chunks.v1`, `d/ciseg.v1`, `d/civec.v1`, `d/ciidx.v1` or `ci_*` rows (today §3.3 says only "reachable closure").
- **§15.2:** add `usearch` (C++ FFI, feature `ann`) to the unsafe/FFI allow-list, and an MSVC CI job for it (and for `onig` if kept).
- **`docs/research/mcp.md`:** extend the M15 `search` row with the additive fields of §3.4; the server advertises `search.v1` from the `CodeIntel` service.
- **PLAN:** add the I-units, phases 4c/5c/9c, and the amendments recorded on U33, U43c, U45, U69 and U73.

---

## 11. Review notes

Critique of 2026-10-22 applied. Items not applied, or applied only in part:

- **Issue 8, `fancy-regex`:** not applied as asked. fastembed 7.2.0 itself enables `tokenizers/onig` (checked on crates.io), so no feature choice here can remove it. I09 decides between keeping `onig` with an MSVC job and bypassing fastembed's tokenizer. `hf-hub` is optional in fastembed and is switched off. The `ort =2.0.0-rc.15` pin and the existence of tree-sitter-tags 0.27.1 were verified.
- **Issue 9, path and existence of a redacted line:** path and line number stay visible, because read access to the repository already allows listing the tree (git-grep parity). `text`, `cols` and context are removed on every non-CLI surface.
- **Issue 11, fork copy:** SPEC §3.3 says only "reachable closure", so the rule is stated here and as a SPEC edit in §10, with test `codeintel_fork` in I15.
- **Issue 12, `main`:** no `main` found, so no change. I17 already depended on U50; the reason (`--json`) is now stated.
