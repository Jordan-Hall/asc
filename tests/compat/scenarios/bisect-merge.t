g init
echo ".p/" > .gitignore
mkdir .p
git add .gitignore
echo 1 > f
git add f
at 1
git commit -qm base
git tag base
git switch -q -c side
echo s1 > s
git add s
at 3
git commit -qm s1
echo s2 >> s
at 5
git commit -qam s2
echo s3 >> s
at 7
git commit -qam s3
git switch -q master
echo m1 > m
git add m
at 2
git commit -qm m1
echo m2 >> m
at 4
git commit -qam m2
at 8
git merge -q --no-ff -m merge side
echo m3 >> m
at 9
git commit -qam m3
echo m4 >> m
at 10
git commit -qam m4
echo m5 >> m
at 11
git commit -qam m5
g bisect start HEAD base
g bisect good
g bisect bad
g bisect good
g bisect good
g bisect log
g bisect reset
