# fidelity: D-3
g init
g config alias.st "status -s"
g config alias.lg "log --oneline"
g config alias.last "log -1 --format=%s"
g config --get alias.st
g config --local --get alias.lg
g config --get-regexp '^alias\.'
g config --list --local
echo a > a.txt
g st
g add a.txt
g st
at 1
g commit -m first
g lg
g last
g config demo.key value
g config demo.key
g config --get demo.key
g config --get demo.missing
g config --unset demo.key
g config demo.key
g config --add demo.multi one
g config --add demo.multi two
g config --get-all demo.multi
g config --bool demo.flag yes
g config --type=bool demo.flag
g config user.name "Config Name"
g config user.email "config@example.com"
g config user.name
echo b > b.txt
g add b.txt
at 2
g commit -m second
g log -1 --format='%an <%ae> | %cn <%ce>'
g config --replace-all demo.multi three
g config --get-all demo.multi
printf '[included]\n\tvalue = from-include\n' > inc.cfg
g config include.path "$(wpwd)/inc.cfg"
g config included.value
printf '[includeIf "gitdir:%s/"]\n\tpath = %s/cond.cfg\n' "$(wpwd)" "$(wpwd)" >> "$HOME/.gitconfig"
printf '[cond]\n\tvalue = matched\n' > cond.cfg
g config cond.value
printf '[includeIf "gitdir:/nowhere/"]\n\tpath = %s/cond2.cfg\n' "$(wpwd)" >> "$HOME/.gitconfig"
printf '[cond]\n\tother = nope\n' > cond2.cfg
g config cond.other
g config --show-scope cond.value
g config --unset-all demo.multi
g config --get-all demo.multi
g config nosuchsection
g config --remove-section demo
