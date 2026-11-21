; The naive Fibonacci: 1,973 calls, and no data structure at all.
; What it allocates is environments, one binding for each call.
(define (fib n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))
(fib 15)
