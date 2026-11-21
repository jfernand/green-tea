; Sixteen slots that live for the whole run. Each step makes a list of three numbers and
; stores it in a slot, in place of the list that was there: a reference to a new object,
; stored in an old one.
(define (slots n acc) (if (= n 0) acc (slots (- n 1) (cons (cons (- n 1) nil) acc))))
(define table (slots 16 nil))
(define (find key t) (if (= key (car (car t))) (car t) (find key (cdr t))))
(define (fill i)
  (set-cdr! (find (remainder (* i 7) 16) table) (cons i (cons i (cons i nil))))
  (if (= i 1) nil (fill (- i 1))))
(fill 250)
(define (total t acc) (if (null? t) acc (total (cdr t) (+ acc (car (cdr (car t)))))))
(total table 0)
