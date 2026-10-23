g init
mkdir -p sub/deep
printf 'alpha one\nbeta two\nAlpha three\n' > a.txt
printf 'gamma\nalpha again\n' > sub/b.txt
printf 'alpha\n' > sub/deep/c.txt
printf 'bin\0alpha\n' > blob.bin
g add .
at 1
g commit -m first
g tag one
printf 'alpha changed\n' > a.txt
echo extra > e.txt
g add -A
at 2
g commit -m second
g grep alpha HEAD
g grep -n alpha one
g grep -i -n alpha HEAD~1
g grep -l alpha HEAD
g grep -c alpha one HEAD
g grep -w alpha one -- sub
g grep -F -e alpha -e gamma HEAD -- sub
g grep -v alpha HEAD -- a.txt
g grep -q alpha HEAD
g grep nomatch HEAD
g grep alpha HEAD -- 'sub/*.txt'
g grep alpha nosuchrev --
cd sub
g grep alpha HEAD
g grep -n gamma one -- .
