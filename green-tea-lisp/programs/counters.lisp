; The table of alist.lisp, kept in place: sixteen counters that live for the whole run,
; each a pair (key . count) whose cdr is overwritten. What is stored is an integer.
(define (counters n acc) (if (= n 0) acc (counters (- n 1) (cons (cons (- n 1) 0) acc))))
(define table (counters 16 nil))
(define (find key t) (if (= key (car (car t))) (car t) (find key (cdr t))))
(define (add1 cell) (set-cdr! cell (+ 1 (cdr cell))))
(define (count i)
  (add1 (find (remainder (* i 7) 16) table))
  (if (= i 1) nil (count (- i 1))))
(count 250)
(define (total t acc) (if (null? t) acc (total (cdr t) (+ acc (cdr (car t))))))
(total table 0)
