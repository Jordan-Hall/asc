g init
printf 'one\ntwo\nthree\nfour\n' > a.txt
g add a.txt
at 1
g commit -m first
masked() { sed -E 's/[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2} [+-][0-9]{4}/DATE/; s/^((author|committer)-time) [0-9]+$/\1 T/'; }
g blame a.txt | masked
printf 'one\nTWO\nthree\nnew\nfour\n' > a.txt
g blame a.txt | masked
g blame -L 2,4 a.txt | masked
g blame --porcelain a.txt | masked
g blame --line-porcelain -L 4,4 a.txt | masked
g blame --incremental a.txt | masked
g blame HEAD a.txt
printf '  one\ntwo\nthree\nfour\n' > a.txt
g blame a.txt | masked
g blame -w a.txt | masked
rm a.txt
g blame a.txt | masked
printf 'x\n' > b.txt
g blame b.txt
g add b.txt
g blame b.txt | masked
g blame --porcelain b.txt | masked
