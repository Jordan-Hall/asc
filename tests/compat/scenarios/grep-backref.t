# `grep` with basic-expression back-references: repeated words, case folding, multiple patterns with
# their own group numbers and a backslash digit inside a bracket expression (a literal there).
# The extended forms are in grep-backref-ere.t.
g init
printf 'the the cat\nabab\nAbab here\nno repeat\nxyzzyx\nfoo bar foo\nworld1\n' > a.txt
printf 'ab-ab\nabcabc\ncd cd\nmixed Cd cd\n' > b.txt
g add a.txt b.txt
at 1
g commit -q -m first

g grep -n '\(the\) \1'
g grep -n '\(ab\)\1'
g grep -n -i '\(ab\)\1'
g grep -n -e '\(cd\) \1' -e '\(abc\)\1'
g grep -n 'world[\1]'
