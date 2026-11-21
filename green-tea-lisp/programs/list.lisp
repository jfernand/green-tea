; Builds the list (1 2 ... 100), reverses it and adds it up, ten times over.
; Every pair is garbage by the time the next round starts.
(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc))))
(define (reverse l acc) (if (null? l) acc (reverse (cdr l) (cons (car l) acc))))
(define (sum l acc) (if (null? l) acc (sum (cdr l) (+ acc (car l)))))
(define (rounds k total)
  (if (= k 0) total
      (rounds (- k 1) (+ total (sum (reverse (iota 100 nil) nil) 0)))))
(rounds 10 0)
