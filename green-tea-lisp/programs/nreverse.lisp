; Reverses a list of 200 in place, 20 times, and adds it up. No pair of the list is ever
; garbage: each round turns every cdr round to point at the pair that was before it.
(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc))))
(define (nreverse l acc)
  (if (null? l) acc
      ((lambda (rest) (set-cdr! l acc) (nreverse rest l)) (cdr l))))
(define (sum l acc) (if (null? l) acc (sum (cdr l) (+ acc (car l)))))
(define (rounds k l) (if (= k 0) (sum l 0) (rounds (- k 1) (nreverse l nil))))
(rounds 20 (iota 200 nil))
