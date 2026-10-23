g init
msg='subject

body text

Signed-off-by: A <a@example.com>
'
printf '%s' "$msg" | git interpret-trailers --trailer 'Reviewed-by: R <r@example.com>'
printf '%s' "$msg" | git interpret-trailers --trailer 'Signed-off-by=X <x@example.com>' --trailer 'Fixes: #1'
printf 'subject\n' | git interpret-trailers --trailer 'A: 1'
printf 'subject\n\nbody\n' | git interpret-trailers --trailer 'A: 1' --trailer 'B: 2'
printf '' | git interpret-trailers --trailer 'B: 1'
printf 'subject\n\nbody\n\nA: 1\n' | git interpret-trailers --trailer 'A: 1'
printf 'subject\n\nbody\n\nA: 1\nB: 2\nA: 1\n' | git interpret-trailers --trailer 'A: 1'
printf 'subject\n\nbody\n\nA: 1\nB: 2\n' | git interpret-trailers --if-exists replace --trailer 'A: 3'
printf 'subject\n\nbody\n\nA: 1\nB: 2\n' | git interpret-trailers --where start --trailer 'A: 3'
printf 'subject\n\nbody\n\nA: 1\nB: 2\nA: 5\n' | git interpret-trailers --where before --trailer 'A: 3'
printf 'subject\n\nbody\n\nA: 1\nB: 2\n' | git interpret-trailers --if-exists add --where end --trailer 'A: 3'
printf 'subject\n\nbody\n\nA: 1\nB: 2\n' | git interpret-trailers --if-missing doNothing --trailer 'C: 3'
printf 'subject\n\nbody\n\nA: 1\nB: 2\nA: 3\n' | git interpret-trailers --if-exists addIfDifferent --trailer 'A: 1'
printf 'subject\n\nbody\n\nA: 1\nB: 2\nA: 3\n' | git interpret-trailers --if-exists doNothing --trailer 'A: 1'
printf 'subject\n\nbody\n\nA: 1\nB: 2\nA: 3\n' | git interpret-trailers --if-exists replace --where start --trailer 'A: 4'
printf 'subject\n\nbody\n\nA: 1\nB: 2\nA: 3\n' | git interpret-trailers --trailer 'A: 3'
printf 'subject\n\nbody\n\nA: 1\n' | git interpret-trailers --trailer 'a: 1'
printf 'subject\n\nbody\n\nA: 1\n' | git interpret-trailers --trailer 'x' --trailer 'y = 3' --trailer 'z=4: 5'
printf 'subject\n\nbody\n\nA: 1\nB: 2\n' | git interpret-trailers --trailer 'A:' --trim-empty
printf 'subject\n\nbody\n\nA: 1\nB:\n' | git interpret-trailers --trim-empty
printf 'subject\n\nbody\n\nA: 1\nB: 2\n' | git interpret-trailers --only-trailers
printf 'subject\n\nbody\n\nA: 1\n  cont\nB: 2\n' | git interpret-trailers --parse
printf 'subject\n\nbody\n\nA: 1\n  cont\nB: 2\n' | git interpret-trailers --only-trailers
printf 'subject\n\nbody\n\nA: 1\n  cont\nB: 2\n' | git interpret-trailers --unfold --trailer 'C: 3'
printf 'subject\n\nbody\n---\n diff\n' | git interpret-trailers --trailer 'A: 1'
printf 'subject\n\nbody\n---\n diff\n' | git interpret-trailers --trailer 'A: 1' --no-divider
printf 'subject\n\nbody\n\nA: 1' | git interpret-trailers --trailer 'B: 1'
printf 'subject\n\n# comment\n' | git interpret-trailers --trailer 'B: 1'
printf 'subject\n\nbody\n\nA: 1\n' | git interpret-trailers --trailer 'A: 3' --trailer 'C: 3' --where start --trailer 'D: 4' --trailer 'E: 5'
printf 'subject\n\nbody\n\nA: 1\n' | git interpret-trailers
printf 'subject\n\nnot a trailer: just prose here\n' | git interpret-trailers --trailer 'K: v'
printf 'a\n\nb: 1\n' > m.txt
printf 'second\n' > n.txt
g interpret-trailers --trailer 'c: 2' m.txt n.txt
g interpret-trailers --in-place --trailer 'c: 2' m.txt
cat m.txt
g interpret-trailers --trailer 'c: 2' nonexist.txt
g config trailer.sign.key 'Signed-off-by: '
g config trailer.ack.key 'Acked-by'
g config trailer.fix.where start
g config trailer.where after
printf 'subject\n\nbody\n\nQ: 1\nR: 2\n' | git interpret-trailers --trailer 'sign=Me <me@example.com>' --trailer 'ack: You' --trailer 'fix=1'
printf 'subject\n\nbody\n\nQ: 1\nR: 2\n' | git interpret-trailers --only-input --trailer 'sign=Me'
