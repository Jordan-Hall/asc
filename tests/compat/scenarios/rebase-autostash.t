g init
printf 'a\nb\nc\n' > f
echo k > k
g add .
at 1
g commit -m base
g switch -c side
echo 1 > m1
g add m1
at 2
g commit -m m1
g switch master
echo x > x
g add x
at 3
g commit -m "main x"
g switch side
echo dirty >> k
g rebase master
at 4
g rebase --autostash master
g status --short
g log --oneline
g switch -c up side
echo more >> k
g rebase --autostash master
g status --short
g rebase --autostash master
