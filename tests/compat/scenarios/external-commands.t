mkdir ../bin
printf '#!/bin/sh\necho "hello from external: $*"\n' > ../bin/git-hello
chmod +x ../bin/git-hello
PATH="$(cd .. && pwd)/bin:$PATH"
export PATH
g init
g hello world one
g config alias.hello "log -1 --format=alias-wins"
echo a > a.txt
g add a.txt
at 1
g commit -m first
g hello
g config alias.graph "log --oneline --decorate"
g graph
g config alias.shell "!echo shell-alias"
g shell
g frobnicate
g config alias.loop "loop"
g loop
