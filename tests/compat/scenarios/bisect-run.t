g init
echo ".p/" > .gitignore
mkdir .p
git add .gitignore
i=1
while [ $i -le 10 ]; do
  echo $i > f
  git add f
  at $i
  git commit -qm "c$i"
  i=$((i+1))
done
cat > .p/lt.sh <<'SCRIPT'
#!/bin/sh
v=$(cat f)
[ "$v" -lt "$1" ]
SCRIPT
cat > .p/crash.sh <<'SCRIPT'
#!/bin/sh
exit 200
SCRIPT
g bisect run true
g bisect start HEAD HEAD~9
g bisect run sh .p/lt.sh 4
g bisect log
g bisect reset
g bisect start HEAD HEAD~9
g bisect run sh .p/crash.sh
g bisect log
g bisect reset
g bisect run
