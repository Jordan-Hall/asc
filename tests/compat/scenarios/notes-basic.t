g init
printf 'one\n' > a.txt
g add a.txt
at 1
g commit -m one
printf 'two\n' >> a.txt
at 2
g commit -am two
g notes add -m "first note" HEAD~1
g notes show HEAD~1
g notes show HEAD
g notes list
g notes list HEAD~1
g notes append -m "more" HEAD~1
g notes show HEAD~1
g notes copy HEAD~1 HEAD
g notes show HEAD
g notes list
at 3
printf 'three\n' >> a.txt
g commit -am three
g log --format='%h %s %N'
g log --notes --format=%s
g notes remove HEAD~1
g notes list
g notes show HEAD~1
g log --format='%h %s %N'
