# A checkout writes the line endings git's attributes and core.autocrlf/core.eol ask for, status
# then reads the files as clean, and commit -a, restore and diff read them back the same way.
# The index is dated after the files before each commit -a, so git has no racily clean entry to
# read again (and warn about) and the output does not depend on timing.
settle() { if [ -d .git ]; then touch -c -d @2000000000 .git/index; fi; }
g init
printf '* text=auto\n*.bin -text\nunix.sh text eol=lf\nwin.bat eol=crlf\n' > .gitattributes
printf 'a\nb\n' > notes.txt
printf 'echo\n' > unix.sh
printf 'echo\n' > win.bat
printf 'a\nb\0' > blob.bin
printf 'one\r\ntwo\n' > mixed.txt
g -c core.eol=lf add .
at 1
g commit -q -m base
g branch other
printf 'a\nb\nc\n' > notes.txt
settle
g -c core.eol=lf commit -q -a -m more

# Switching writes CRLF for text under core.eol=crlf, and LF where eol=lf says so.
g -c core.eol=crlf switch -q other
g -c core.eol=crlf status --porcelain
g -c core.eol=crlf diff --stat
g -c core.eol=crlf switch -q master
g -c core.eol=crlf status --porcelain

# core.autocrlf=true converts files without a text attribute too.
printf 'plain\n' > plain.dat
g -c core.eol=crlf add plain.dat
at 2
g -c core.eol=crlf commit -q -m plain
rm plain.dat notes.txt
g -c core.autocrlf=true restore plain.dat notes.txt
g -c core.autocrlf=true status --porcelain

# commit -a reads the CRLF work tree back to the LF it stored.
printf 'a\r\nb\r\nc\r\nd\r\n' > notes.txt
settle
g -c core.eol=crlf commit -q -a -m crlf-edit
g show --stat --format=%s HEAD
g -c core.eol=crlf status --porcelain

# A checkout converts by the attributes of the tree it writes, not those it replaces.
g switch -q -c raw
printf '* text=auto\n*.bin -text\nunix.sh text eol=lf\nwin.bat eol=crlf\nnotes.txt -text\n' > .gitattributes
printf 'z\n' > notes.txt
g add .gitattributes notes.txt
at 3
g commit -q -m raw
g -c core.eol=crlf switch -q master
g -c core.eol=crlf status --porcelain
g -c core.eol=crlf switch -q raw
g -c core.eol=crlf status --porcelain

# stash push -u removes an untracked .gitattributes before it writes the files back.
mkdir d
printf 'f\n' > d/f.txt
g add d/f.txt
at 4
g commit -q -m d
printf '* text eol=crlf\n' > d/.gitattributes
printf 'changed\n' > d/f.txt
settle
g -c core.eol=lf stash push -q -u
g status --porcelain
