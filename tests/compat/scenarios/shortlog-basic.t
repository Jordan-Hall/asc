g init
printf 'x\n' > a.txt
g add a.txt
GIT_AUTHOR_NAME=Alice GIT_AUTHOR_EMAIL=a@example.com GIT_COMMITTER_NAME=Alice GIT_COMMITTER_EMAIL=a@example.com
export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL
at 1
g commit -q -m "first change"
GIT_AUTHOR_NAME=Bob GIT_AUTHOR_EMAIL=b@example.com GIT_COMMITTER_NAME=Bob GIT_COMMITTER_EMAIL=b@example.com
printf 'y\n' >> a.txt
at 2
g commit -q -am "second

body line"
GIT_AUTHOR_NAME=Alice GIT_AUTHOR_EMAIL=alice2@example.com GIT_COMMITTER_NAME=Alice GIT_COMMITTER_EMAIL=alice2@example.com
printf 'z\n' >> a.txt
at 3
g commit -q -am "[PATCH 1/2] third: a long subject line to see wrapping behaviour of shortlog when asked"
GIT_AUTHOR_NAME="Carol Z" GIT_AUTHOR_EMAIL=c@example.com GIT_COMMITTER_NAME=Dave GIT_COMMITTER_EMAIL=d@example.com
printf 'w\n' >> a.txt
at 4
g commit -q -am "fourth

Co-authored-by: Eve <e@example.com>
Reviewed-by: Zed <z@example.com>"
g shortlog HEAD
g shortlog -s HEAD
g shortlog -n HEAD
g shortlog -ne HEAD
g shortlog -sne HEAD
g shortlog -sc HEAD
g shortlog -c HEAD
g shortlog -w20,2,4 HEAD
g shortlog -w HEAD
g shortlog --group=trailer:co-authored-by HEAD
g shortlog --group=author --group=committer -s HEAD
g shortlog -e -s HEAD
g shortlog -n HEAD~1..HEAD
g shortlog HEAD -- nothing
g shortlog HEAD -- a.txt
g shortlog --since=1970-01-01 HEAD
g shortlog -2 HEAD
g shortlog --author=Alice HEAD
g shortlog --numbered --summary --email HEAD
git log --pretty=short | git shortlog -s
git log | git shortlog
git log | git shortlog -ne
printf '' | git shortlog
g whatchanged
g whatchanged --i-still-use-this
g whatchanged --i-still-use-this --oneline
g whatchanged --i-still-use-this -2
g whatchanged --i-still-use-this HEAD~2..
g whatchanged --i-still-use-this --oneline -- a.txt
g whatchanged --i-still-use-this --author=Bob
printf '.mailmap\n' > /dev/null
printf 'Alice Main <a@example.com> <alice2@example.com>\n' > .mailmap
g shortlog -se HEAD
g shortlog -s HEAD
