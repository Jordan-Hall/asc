g init
mkdir -p .hooks
g config core.hooksPath "$(wpwd)/.hooks"
cat > .hooks/reference-transaction <<'HOOK'
#!/bin/sh
# Branch updates only, in the prepared and committed states. git also runs the hook for
# `preparing`, ORIG_HEAD and AUTO_MERGE; those are not part of the contract compared here.
case "$1" in prepared|committed) ;; *) cat > /dev/null; exit 0 ;; esac
while read old new ref; do
  case "$ref" in refs/heads/*) echo "ref-txn $1 $old $new $ref" >&2 ;; esac
done
HOOK
chmod +x .hooks/reference-transaction
echo one > a.txt
g add a.txt
at 1
g commit -m first
g branch topic
echo two >> a.txt
g add a.txt
at 2
g commit -m second
g reset --soft HEAD~1
