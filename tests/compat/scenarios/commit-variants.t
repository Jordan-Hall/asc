g init
echo one > a.txt
g add a.txt
at 1
g commit -m "first" -m "second paragraph"
g log --format=%B
echo two >> a.txt
at 2
g commit -a --amend -m "amended"
g log --format=%H%n%B
printf 'from file\n\nbody line\n' > msg.txt
echo three >> a.txt
at 3
g commit -a -F msg.txt
g log -n1 --format=%B
at 4
g commit --allow-empty -m "empty one"
at 5
g commit --allow-empty-message --allow-empty -m ""
g commit -m "nothing changed"
echo four >> a.txt
at 6
g commit -a --author="Other Person <other@example.com>" -m "other author"
g log -n1 --format='%an <%ae> / %cn <%ce>'
echo five >> a.txt
at 7
g commit -a --date="2020-01-02T03:04:05+0000" -m "dated"
g log -n1 --format='%ad | %cd' --date=iso
at 8
g commit --amend --no-edit --allow-empty
g log --format='%h %s' 
printf '  spaced   message  \n\n\n' > ws.txt
echo six >> a.txt
at 9
g commit -a -F ws.txt
g log -n1 --format=%B
at 11
g commit --fixup=HEAD --allow-empty
g log -n1 --format=%s
g commit -q --allow-empty -m quiet
echo eight >> a.txt
g commit
g add a.txt
g commit
at 12
g commit --amend
g log -n1 --format=%B
g commit --allow-empty -e -m "edited template"
