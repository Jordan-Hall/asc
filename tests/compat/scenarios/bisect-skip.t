g init
echo ".p/" > .gitignore
mkdir .p
git add .gitignore
i=1
while [ $i -le 9 ]; do
  echo $i > f
  git add f
  at $i
  git commit -qm "c$i"
  git tag t$i
  i=$((i+1))
done
g bisect start HEAD HEAD~8
# Skipping a commit that is not the next one changes nothing but the record.
g bisect skip t3
g bisect skip t7 t2
g bisect log
g rev-parse --short HEAD
g bisect good
g bisect skip t6
g bisect log
g bisect good
g bisect skip
g bisect log
g bisect reset
# Skipping the only candidate left.
g bisect start t9 t7
g bisect skip
g bisect log
g bisect reset
