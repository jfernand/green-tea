; An association list used as a table of 16 counters. An update makes a new list, which
; shares with the old one every pair after the one that changed.
(define (lookup key table)
  (if (null? table) 0
      (if (= key (car (car table))) (cdr (car table))
          (lookup key (cdr table)))))
(define (update key value table)
  (if (null? table) (cons (cons key value) nil)
      (if (= key (car (car table))) (cons (cons key value) (cdr table))
          (cons (car table) (update key value (cdr table))))))
(define (count i table)
  (if (= i 0) table
      (count (- i 1)
             ((lambda (key) (update key (+ 1 (lookup key table)) table))
              (remainder (* i 7) 16)))))
(define (total key table acc)
  (if (= key 16) acc (total (+ key 1) table (+ acc (lookup key table)))))
(total 0 (count 250 nil) 0)
