g init
echo one > a.txt
g add a.txt
at 1
g commit -m one
g tag v1
g tag
echo two >> a.txt
at 2
g commit -am two
g tag -a v2 -m "release two"
g tag -l
g tag -l 'v*'
g tag -n
g tag -n1
g tag v1.5 HEAD~1
g tag --list --sort=-refname
g rev-parse v1 v2 v1.5
g rev-parse v2^{}
g cat-file -t v2
g cat-file -p v2
g cat-file -t v1
g tag v1
g tag -f v1 HEAD
g rev-parse v1
g tag -d v1.5
g tag -d nosuch
g tag -a v3 -m "three" HEAD~1
g tag --contains HEAD~1
g tag --points-at HEAD
g tag -m nomessage
g tag -a noeditor
g log --oneline --decorate
g tag -a -m "multi
line" v4
g cat-file -p v4
g show-ref --tags
g describe
g describe --tags
