g init
echo base > f
g add .
at 1
g commit -m base
g tag base
g switch -c side
echo change > f
at 2
g commit -am "change f"
g switch master
echo change > f
at 3
g commit -am "same change"
g cherry-pick side
g status
g cherry-pick --skip
g status
g cherry-pick --keep-redundant-commits side
g log --oneline
g commit --allow-empty -m "an empty commit"
g switch side
g cherry-pick master
g cherry-pick --allow-empty master
g log --oneline
