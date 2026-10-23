g init
seq 1 20 | sed 's/^/line /' > f
g add f
at 1
g commit -m base
g checkout -q -b up
echo up1 > u1
g add u1
at 2
g commit -m "up one"
echo hello > g
g add g
at 3
g commit -m "shared change"
echo up3 > u3
g add u3
at 4
g commit -m "up three"
g checkout -q -b topic master
echo top1 > t1
g add t1
at 5
g commit -m "topic one"
g cherry-pick up~1
echo top3 > t3
g add t3
at 6
g commit -m "topic three"
g checkout -q up
g checkout -q topic
g cherry up topic
g cherry -v up topic
g cherry up
g cherry -v up HEAD
g cherry up topic topic~1
g cherry -v up topic topic~2
g cherry --abbrev=7 up topic
g cherry -v --abbrev up topic
g cherry topic up
g cherry -v topic up
g cherry
g branch --set-upstream-to=up
g cherry -v
g cherry nosuch
g cherry up nosuch
g cherry up topic nosuch
g cherry up topic topic~1 extra
g checkout -q -b side master
echo other > o1
g add o1
at 7
g commit -m "side one"
g merge -q --no-ff up -m "merge up"
echo more > m1
g add m1
at 8
g commit -m "side more"
g cherry -v up side
g cherry -v topic side
