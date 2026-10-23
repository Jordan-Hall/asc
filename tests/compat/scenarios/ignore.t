g init
printf '*.log\nbuild/\n!keep.log\n/root-only.txt\n' > .gitignore
mkdir build sub
echo x > build/out.bin
echo x > debug.log
echo x > keep.log
echo x > root-only.txt
echo x > sub/root-only.txt
echo x > sub/inner.log
printf '*.tmp\n!important.tmp\n' > sub/.gitignore
echo x > sub/a.tmp
echo x > sub/important.tmp
echo x > normal.txt
g status
g status -s
g status --ignored
g status --porcelain=v2
g add .
g status -s
g add debug.log
g add -f debug.log
g status -s
g add build/out.bin
g ls-files
at 1
g commit -m ignored
g status -s
echo y >> debug.log
g status -s
g ls-files --others --exclude-standard
g rm --cached debug.log
g status -s
