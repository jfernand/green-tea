; A function that calls itself through the pair it is stored in. The function's
; environment holds the pair, and the pair holds the function: a cycle of three objects,
; made 100 times.
(define (make-adder)
  ((lambda (cell)
     (set-car! cell (lambda (n acc) (if (= n 0) acc ((car cell) (- n 1) (+ acc n)))))
     (car cell))
   (cons nil nil)))
(define (repeat k total)
  (if (= k 0) total (repeat (- k 1) (+ total ((make-adder) 10 0)))))
(repeat 100 0)
