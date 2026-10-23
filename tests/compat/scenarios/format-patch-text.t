g init
echo ".p/" > .gitignore
echo base > f
g add .
at 1
g commit -m base
g tag base
echo caf > g
git add g
at 2
g commit -m "Caf$(printf '\303\251') na$(printf '\303\257')ve subject

Body with $(printf '\303\251') in it."
echo 2 >> g
at 3
g commit -am "Subject with = and ? characters =?x?"
echo 3 >> g
at 4
GIT_AUTHOR_NAME="Doe, Jane" g commit -am "comma name"
echo 4 >> g
at 5
GIT_AUTHOR_NAME="Ren$(printf '\303\251') Auth" g commit -am "accent name"
g format-patch --stdout base
