g init
printf 'a\nb\nc\n' > f
printf 'bin\0one' > b.bin
g add .
at 1
g commit -m base
g tag v0
g switch -c side
printf 'a\nb\nside\n' > f
printf 'bin\0side' > b.bin
echo s > s.txt
g add -A
at 2
g commit -m "side change"
echo more >> s.txt
at 3
g commit -am "side two"
g tag v1
g switch master
printf 'main\nb\nc\n' > f
printf 'bin\0main' > b.bin
at 4
g commit -am "main change"
g merge --squash side
g status
cat f
g reset --hard HEAD
g merge v1
g status
g merge --abort
g switch -c feature
g merge --no-ff -m "first line" -m "second paragraph" v0
g log -n1 --format=%B
g switch -c topic v0
echo t > t.txt
g add t.txt
at 5
g commit -m topic
g merge side
g merge --abort
g merge -X ours side
g status
g merge --abort
