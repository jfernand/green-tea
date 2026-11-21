; A queue: a pair whose car is the first pair of a list and whose cdr is the last. Five
; hundred numbers go through it, with never more than eight waiting, and are added up as they
; come out. The pair that is the queue lives for the whole run, and is written all the time.
(define (enqueue q x)
  ((lambda (cell)
     (if (null? (car q)) (set-car! q cell) (set-cdr! (cdr q) cell))
     (set-cdr! q cell))
   (cons x nil)))
(define (dequeue q)
  ((lambda (first) (set-car! q (cdr first)) (car first)) (car q)))
(define (drain q acc) (if (null? (car q)) acc (drain q (+ acc (dequeue q)))))
(define (feed q i n acc)
  (if (< n i) (drain q acc)
      ((lambda (ignored) (feed q (+ i 1) n (if (< 8 i) (+ acc (dequeue q)) acc)))
       (enqueue q i))))
(feed (cons nil nil) 1 500 0)
