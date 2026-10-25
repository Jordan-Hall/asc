# fuzz regression, seed 20261023 (docs/git-fidelity.md, Q04): a difference no register entry covers.
# first differing section: stderr|reset --hard master~1
# 4 of 28 steps remain after shrinking.
# differential fuzz script (tests/compat/src/fuzz)
op() { printf '[op %s]\n' "$1"; printf '[op %s]\n' "$1" >&2; }
walk() {
  for p in "$1"* "$1".[!.]* "$1"..?*; do
    case "$p" in .git) continue ;; esac
    if [ -L "$p" ]; then printf 'l %s -> %s\n' "$p" "$(readlink "$p")"
    elif [ -d "$p" ]; then walk "$p/"
    elif [ -f "$p" ]; then
      if [ -x "$p" ]; then m=x; else m=-; fi
      printf 'f %s %s\n' "$m" "$p"
      printf '%s\000' "$p" >> ../fuzz-files
    fi
  done
}
tree() {
  printf '[tree]\n'
  : > ../fuzz-files
  walk ''
  xargs -r -0 cksum < ../fuzz-files
}
unlink_dangling() {
  for p in "$1"* "$1".[!.]* "$1"..?*; do
    case "$p" in .git) continue ;; esac
    if [ -L "$p" ]; then [ -e "$p" ] || rm -f "$p"
    elif [ -d "$p" ]; then unlink_dangling "$p/"
    fi
  done
}
probe() {
  g status --porcelain=v2 --branch
  g log --all -n 40 --format='%H %T %P %D %s'
  g diff --raw
  g diff --cached --raw
  g ls-files -s
  g for-each-ref
  tree
}
g init
op 0
at 1
printf 'line 0\nline 1\nline 2\nline 3' > 'b.txt'
op 1
at 2
g 'add' '-A'
probe
op 2
at 3
g 'commit' '-a' '-m' 'm2'
probe
op 3
at 4
g 'reset' '--hard' 'master~1'
probe
unlink_dangling ''
