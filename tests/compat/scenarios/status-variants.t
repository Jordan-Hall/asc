g init
echo base > tracked.txt
echo keep > keep.txt
mkdir dir
echo inner > dir/inner.txt
g status
g status -s
g add -A
at 1
g commit -m base
g status
echo changed >> tracked.txt
echo fresh > untracked.txt
echo staged > staged.txt
g add staged.txt
rm keep.txt
g status
g status -s
g status --short --branch
g status --porcelain
g status --porcelain=v2
g status --porcelain=v2 --branch
g status --porcelain=v2 -z
g status -uno
g status -s -unormal
g status -sb dir
g add tracked.txt
echo more >> tracked.txt
g status -s
g status --porcelain=v1 -z
g mv dir/inner.txt dir/renamed.txt
g status
g status -s
g status --porcelain=v2
g rm --cached staged.txt
g status -s
g rm untracked.txt
g rm -f dir/renamed.txt
g status -s
g status --ignored
