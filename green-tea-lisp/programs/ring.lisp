; Makes 100 rings: lists of 8 whose last pair points back at the first. Each ring is
; walked twice round and then dropped.
(define (iota n acc) (if (= n 0) acc (iota (- n 1) (cons n acc))))
(define (last l) (if (null? (cdr l)) l (last (cdr l))))
(define (ring n) ((lambda (l) (set-cdr! (last l) l) l) (iota n nil)))
(define (walk l k acc) (if (= k 0) acc (walk (cdr l) (- k 1) (+ acc (car l)))))
(define (rings k total)
  (if (= k 0) total (rings (- k 1) (+ total (walk (ring 8) 16 0)))))
(rings 100 0)
