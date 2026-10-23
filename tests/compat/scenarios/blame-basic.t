g init
printf 'one\ntwo\nthree\nfour\n' > a.txt
g add a.txt
g blame a.txt
at 1
g commit -m first
export GIT_AUTHOR_NAME="Zed Longname" GIT_AUTHOR_EMAIL=zed@example.com
printf 'one\nTWO\nthree\nfour\nfive\n' > a.txt
at 2
g commit -am second -m "second body"
export GIT_AUTHOR_NAME="Al" GIT_AUTHOR_EMAIL=al@example.com
printf 'one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven' > a.txt
g add a.txt
at 3
g commit -m third
g blame a.txt
g blame -- a.txt
g blame HEAD a.txt
g blame HEAD~1 a.txt
g blame HEAD~1 -- a.txt
g blame HEAD~2 a.txt
g blame --root HEAD~2 a.txt
g blame -L 2,3 a.txt
g blame -L2,3 a.txt
g blame -L 2,+2 a.txt
g blame -L 5,-2 a.txt
g blame -L 5,2 a.txt
g blame -L 5 a.txt
g blame -L ,2 a.txt
g blame -L 10,99 a.txt
g blame -L 1,2 -L 4,5 a.txt
g blame -L 1,3 -L 2,5 a.txt
g blame --porcelain a.txt
g blame -p -L 3,6 a.txt
g blame --line-porcelain -L 2,4 a.txt
g blame --incremental a.txt
g blame --incremental -L 3,6 HEAD~1 a.txt
g blame --porcelain HEAD~2 a.txt
g blame -w a.txt
mkdir sub
(cd sub && g blame ../a.txt)
(cd sub && g blame -L 1,1 -- ../a.txt)
g blame nosuch.txt
g blame HEAD nosuch.txt
g blame nosuchrev a.txt
g blame -L 99 a.txt
g blame -L 0,1 a.txt
g blame -L x a.txt
g blame -L 99,100 a.txt
g blame sub
: > empty.txt
g add empty.txt
at 4
g commit -m empty
g blame empty.txt
