# git warns when a checkout would not give a file's line endings back: add, stash push -u and
# diff repeat it for text=auto, the eol attribute and core.autocrlf, and say nothing for binary
# files, CRLFs already staged under auto, or paths nothing converts.
g init
printf 'plain\n' > untouched.txt
g add untouched.txt
at 1
g commit -q -m base
printf '* text=auto\n*.bin -text\nstrict.txt text eol=lf\nwin.txt eol=crlf\n' > .gitattributes
printf 'a\nb\n' > lf.txt
printf 'a\r\nb\n' > mixed.txt
printf 'a\r\nb\r\n' > strict.txt
printf 'a\nb\n' > win.txt
printf 'a\nb\0' > blob.bin
g -c core.eol=lf add .gitattributes lf.txt mixed.txt strict.txt win.txt blob.bin
g -c core.eol=crlf add -n lf.txt
at 2
g commit -q -m attrs

# A staged file with CRLFs keeps them under auto, so its next change is not reported.
printf 'a\r\nb\r\n' > crlf.txt
printf 'crlf.txt -text\n' >> .gitattributes
g add .gitattributes crlf.txt
at 3
g commit -q -m crlf
printf '* text=auto\n*.bin -text\nstrict.txt text eol=lf\nwin.txt eol=crlf\n' > .gitattributes
printf 'a\r\nb\r\nc\r\n' > crlf.txt
g -c core.eol=lf add crlf.txt
g -c core.eol=lf diff --cached --stat

# diff reads the changed work tree files the same way.
printf 'a\r\nb\r\nc\r\n' > strict.txt
printf 'a\nb\nc\n' > lf.txt
g -c core.eol=crlf diff --stat
g -c core.autocrlf=true diff --stat
g -c core.eol=lf diff --stat

# stash push -u reads the untracked files first, then the changed tracked ones.
printf 'new\n' > new.txt
printf 'x\r\ny\r\n' > other.txt
printf 'other.txt text eol=lf\n' >> .gitattributes
g -c core.eol=crlf stash push -q -u
g status --porcelain
g -c core.eol=crlf stash pop -q
g status --porcelain

# The legacy crlf attribute: crlf=input stores LF and checks out LF whatever core.eol says, and
# -crlf leaves the file alone.
printf 'legacy.txt crlf=input\nraw.txt -crlf\n' >> .gitattributes
printf 'a\r\nb\r\n' > legacy.txt
printf 'a\r\nb\n' > raw.txt
g -c core.eol=crlf add legacy.txt raw.txt
g diff --cached --stat
