g init
echo ".p/" > .gitignore
mkdir .p
git add .gitignore
i=1
while [ $i -le 8 ]; do
  echo $i > f
  git add f
  at $i
  git commit -qm "c$i"
  i=$((i+1))
done
git tag good1 HEAD~7
g bisect
g bisect foo
g bisect log
g bisect good
g bisect bad
g bisect reset
g bisect start
g bisect log
g bisect bad
g bisect good HEAD~7
g rev-parse --short HEAD
g bisect good
g rev-parse --short HEAD
g bisect bad
g bisect good
g bisect log
g rev-parse --short HEAD
g bisect reset
g rev-parse --abbrev-ref HEAD
g bisect log
g bisect reset
# With revisions on the command line, detached start, and a new start while bisecting.
git checkout -q --detach HEAD~1
g bisect start master good1
g bisect bad HEAD~1 HEAD
g bisect start master good1
g bisect reset
g rev-parse --short HEAD
git checkout -q master
