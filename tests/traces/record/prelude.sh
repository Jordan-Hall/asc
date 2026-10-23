# Sourced by record.sh before a scenario. Helpers (every driver step goes through one):
#   g <git args>          a git command typed by the user (a failing exit is part of the trace)
#   act '<snippet>' [a..] a non-git action (file edits); the text is stored in the trace
#   tool <name> <cmd..>   run a tool; the git commands it runs directly are the tool's
mark() { git -c "asc.mark=$1" version >/dev/null; }
g() { git "$@" || true; }
act() { node "$ASC_TRACE_REC/act.mjs" "$@"; }
tool() {
  name=$1
  shift
  mark "begin:$name"
  "$@" || true
  mark "end:$name"
}
