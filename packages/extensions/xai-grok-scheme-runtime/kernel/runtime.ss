;;; Hyper scheme extension kernel.
;;;
;;; Gambit-flavored Scheme: the same source runs interpreted under `gsi` /
;;; `gxi` (Gerbil ships the Gambit runtime) and compiled via `gsc -exe`.
;;; Gerbil's gxc module system is deliberately NOT used (its namespacing hides
;;; kernel primitives from interaction-environment eval).
;;;
;;; Wire contract (host = trusted Rust parent):
;;;   stdout carries ONLY 4-byte-LE length-prefixed UTF-8 s-expr frames.
;;;   Diagnostics go to stderr (bounded); uncaught exceptions exit nonzero.
;;;
;;; Host -> kernel ops:
;;;   (hello <protocol-int>)
;;;   (load-plugin "name" "source")
;;;   (dispatch <event-sym> "plugin" ((key value) ...))
;;;   (redefine "plugin" <event-sym> "lambda-source")
;;;   (inspect)
;;;   (eval "source")
;;;   (list-commands)                              -> (commands ((plugin name desc) ...))
;;;   (invoke-command "plugin" "name" "args")      -> (ok "output") | (err "msg")
;;;   (list-tools)                                 -> (tools ((plugin name desc schema) ...))
;;;   (invoke-tool "plugin" "name" "input-json")   -> (ok "output") | (err "msg")
;;;   (quit)
;;;
;;; Handler replies (normalized before hitting the wire):
;;;   (allow) | (deny "reason") | (continue) | (block "reason")
;;;   (inject <string-or-#f> <string-or-#f>) | (ok) | (no-handler) | (err "msg")

(define kernel-protocol-version 1)
(define kernel-version "hyper-scheme-kernel/1")
(define max-frame-bytes (* 4 1024 1024))

;; ---------------------------------------------------------------------------
;; frame io
;; ---------------------------------------------------------------------------

(define (read-exact-u8vector len)
  (let ((v (make-u8vector len)))
    (let loop ((got 0))
      (if (>= got len)
          v
          (let ((n (read-subu8vector v got len (current-input-port))))
            (if (or (not n) (eof-object? n) (= n 0))
                #f
                (loop (+ got n))))))))

(define (utf8-bytes->string v)
  (if (= (u8vector-length v) 0)
      ""
      (call-with-input-u8vector
       (list init: v char-encoding: 'UTF-8)
       (lambda (p)
         (let ((s (read-line p #f)))
           (if (eof-object? s) "" s))))))

(define (string->utf8-bytes s)
  (call-with-output-u8vector
   (list char-encoding: 'UTF-8)
   (lambda (p) (display s p))))

(define (read-frame)
  (let ((hdr (read-exact-u8vector 4)))
    (if (not hdr)
        #f
        (let ((len (+ (u8vector-ref hdr 0)
                      (* 256 (u8vector-ref hdr 1))
                      (* 65536 (u8vector-ref hdr 2))
                      (* 16777216 (u8vector-ref hdr 3)))))
          (cond ((> len max-frame-bytes) (kernel-die "incoming frame too large"))
                ((= len 0) "")
                (else
                 (let ((v (read-exact-u8vector len)))
                   (if (not v) #f (utf8-bytes->string v)))))))))

(define (write-frame s)
  (let* ((v (string->utf8-bytes s))
         (len (u8vector-length v))
         (out (current-output-port)))
    (if (> len max-frame-bytes) (kernel-die "outgoing frame too large"))
    (write-u8 (bitwise-and len 255) out)
    (write-u8 (bitwise-and (arithmetic-shift len -8) 255) out)
    (write-u8 (bitwise-and (arithmetic-shift len -16) 255) out)
    (write-u8 (bitwise-and (arithmetic-shift len -24) 255) out)
    (write-subu8vector v 0 len out)
    (force-output out)))

;; ---------------------------------------------------------------------------
;; canonical s-expr serializer (mirror of the host's strict reader)
;; ---------------------------------------------------------------------------

(define (sexp->wire x)
  (let ((p (open-output-string)))
    (emit-sexp x p)
    (get-output-string p)))

(define (emit-sexp x p)
  (cond ((null? x) (display "()" p))
        ((pair? x) (emit-list x p))
        ((string? x) (emit-string x p))
        ((symbol? x) (display (symbol->string x) p))
        ((and (integer? x) (exact? x)) (display (number->string x) p))
        ((eq? x #t) (display "#t" p))
        ((eq? x #f) (display "#f" p))
        (else (emit-string (object->string x 256) p))))

(define (emit-list x p)
  (display "(" p)
  (let loop ((x x) (first #t))
    (cond ((null? x) (display ")" p))
          ((pair? x)
           (if (not first) (display " " p))
           (emit-sexp (car x) p)
           (loop (cdr x) #f))
          (else
           ;; Improper tails never survive normalize-reply; render defensively.
           (display " " p)
           (emit-sexp x p)
           (display ")" p)))))

(define (emit-string s p)
  (display "\"" p)
  (let ((n (string-length s)))
    (let loop ((i 0))
      (if (< i n)
          (let* ((c (string-ref s i))
                 (code (char->integer c)))
            (cond ((char=? c #\") (display "\\\"" p))
                  ((char=? c #\\) (display "\\\\" p))
                  ((char=? c #\newline) (display "\\n" p))
                  ((char=? c #\return) (display "\\r" p))
                  ((char=? c #\tab) (display "\\t" p))
                  ((< code 32)
                   (display "\\x" p)
                   (display (number->string code 16) p)
                   (display ";" p))
                  (else (write-char c p)))
            (loop (+ i 1))))))
  (display "\"" p))

;; ---------------------------------------------------------------------------
;; diagnostics
;; ---------------------------------------------------------------------------

(define (exception->message e)
  (let ((p (open-output-string)))
    (display-exception e p)
    (let* ((s (get-output-string p))
           (n (string-length s))
           (m (min n 512)))
      ;; Single line, bounded.
      (let ((trimmed (substring s 0 m)))
        (list->string
         (map (lambda (c) (if (or (char=? c #\newline) (char=? c #\return)) #\space c))
              (string->list trimmed)))))))

(define (kernel-die msg)
  (display (string-append "hyper-scheme-kernel fatal: " msg) (current-error-port))
  (newline (current-error-port))
  (force-output (current-error-port))
  (exit 70))

;; (try thunk) -> (ok . value) | (err . message-string)
(define (try thunk)
  (with-exception-catcher
   (lambda (e) (cons 'err (exception->message e)))
   (lambda () (cons 'ok (thunk)))))

;; ---------------------------------------------------------------------------
;; kernel state: plugins + tracked handler bindings
;; ---------------------------------------------------------------------------

(define handlers-table (make-table test: equal?))
;; (plugin name) -> (description . proc); proc takes one string arg.
(define commands-table (make-table test: equal?))
;; (plugin name) -> (description schema-json proc); proc takes one string arg.
(define tools-table (make-table test: equal?))
(define current-plugin #f)
(define loaded-plugins '())

;; Public API surface for plugin scripts.
(define (register-handler! event proc)
  (if (not current-plugin)
      (error "register-handler! called outside plugin load"))
  (if (not (symbol? event))
      (error "register-handler! event must be a symbol"))
  (if (not (procedure? proc))
      (error "register-handler! handler must be a procedure"))
  (table-set! handlers-table (list current-plugin event) proc))

;; (register-command! "name" "description" (lambda (args-string) ...))
;; Handler return: string -> command output; anything else -> "".
(define (register-command! name description proc)
  (if (not current-plugin)
      (error "register-command! called outside plugin load"))
  (if (not (and (string? name) (> (string-length name) 0)))
      (error "register-command! name must be a non-empty string"))
  (if (not (string? description))
      (error "register-command! description must be a string"))
  (if (not (procedure? proc))
      (error "register-command! handler must be a procedure"))
  (table-set! commands-table (list current-plugin name) (cons description proc)))

;; (register-tool! "name" "description" "json-schema" (lambda (input-json) ...))
;; Handler return: string -> tool output; anything else -> "".
(define (register-tool! name description schema proc)
  (if (not current-plugin)
      (error "register-tool! called outside plugin load"))
  (if (not (and (string? name) (> (string-length name) 0)))
      (error "register-tool! name must be a non-empty string"))
  (if (not (string? description))
      (error "register-tool! description must be a string"))
  (if (not (string? schema))
      (error "register-tool! schema must be a JSON string"))
  (if (not (procedure? proc))
      (error "register-tool! handler must be a procedure"))
  (table-set! tools-table (list current-plugin name) (list description schema proc)))

;; ctx helper for plugin authors: ((key value) ...) lookup.
(define (ctx-ref ctx key)
  (let ((e (assq key ctx)))
    (if (and e (pair? (cdr e))) (cadr e) #f)))

;; Substring search helper (Gambit has no portable string-contains).
(define (string-contains? haystack needle)
  (let ((hn (string-length haystack))
        (nn (string-length needle)))
    (if (= nn 0)
        #t
        (let loop ((i 0))
          (cond ((> (+ i nn) hn) #f)
                ((string=? (substring haystack i (+ i nn)) needle) #t)
                (else (loop (+ i 1))))))))

(define (do-load-plugin name source)
  (set! current-plugin name)
  (let ((r (try (lambda ()
                  (eval (with-input-from-string
                            (string-append "(begin #t " source "\n)")
                          read))))))
    (set! current-plugin #f)
    (if (eq? (car r) 'ok)
        (begin
          (if (not (member name loaded-plugins))
              (set! loaded-plugins (cons name loaded-plugins)))
          '(ok))
        (list 'err (cdr r)))))

(define (do-redefine plugin event source)
  (let ((r (try (lambda () (eval (with-input-from-string source read))))))
    (cond ((eq? (car r) 'err) (list 'err (cdr r)))
          ((not (procedure? (cdr r)))
           '(err "redefine source must evaluate to a procedure"))
          (else
           (table-set! handlers-table (list plugin event) (cdr r))
           '(ok)))))

(define (do-inspect)
  (let ((acc '()))
    (table-for-each
     (lambda (k v) (set! acc (cons (list (car k) (cadr k)) acc)))
     handlers-table)
    (list 'handlers acc)))

(define (do-eval source)
  (let ((r (try (lambda ()
                  (eval (with-input-from-string
                            (string-append "(begin #t " source "\n)")
                          read))))))
    (if (eq? (car r) 'ok)
        (list 'ok (object->string (cdr r) 4096))
        (list 'err (cdr r)))))

;; ---------------------------------------------------------------------------
;; registered commands / tools (host collects at boot, invokes on demand)
;; ---------------------------------------------------------------------------

(define (do-list-commands)
  (let ((acc '()))
    (table-for-each
     (lambda (k v)
       (set! acc (cons (list (car k) (cadr k) (car v)) acc)))
     commands-table)
    (list 'commands acc)))

(define (do-list-tools)
  (let ((acc '()))
    (table-for-each
     (lambda (k v)
       (set! acc (cons (list (car k) (cadr k) (car v) (cadr v)) acc)))
     tools-table)
    (list 'tools acc)))

;; Shared invoke path: run proc on one string arg, normalize to (ok "out").
(define (invoke-string-proc proc arg)
  (let ((r (try (lambda () (proc arg)))))
    (cond ((eq? (car r) 'err) (list 'err (cdr r)))
          ((string? (cdr r)) (list 'ok (cdr r)))
          (else '(ok "")))))

(define (do-invoke-command plugin name args)
  (let ((entry (table-ref commands-table (list plugin name) #f)))
    (if (not entry)
        '(err "no such command")
        (invoke-string-proc (cdr entry) args))))

(define (do-invoke-tool plugin name input)
  (let ((entry (table-ref tools-table (list plugin name) #f)))
    (if (not entry)
        '(err "no such tool")
        (invoke-string-proc (caddr entry) input))))

;; ---------------------------------------------------------------------------
;; dispatch + reply normalization
;; ---------------------------------------------------------------------------

(define (wire-atom? x)
  (or (string? x) (symbol? x) (boolean? x) (and (integer? x) (exact? x))))

(define (wire-safe? x)
  (cond ((null? x) #t)
        ((pair? x)
         (and (wire-safe? (car x))
              (let ((tail (cdr x)))
                (or (null? tail) (and (pair? tail) (wire-safe? tail))))))
        (else (wire-atom? x))))

(define allowed-reply-heads '(allow deny continue block inject ok no-handler err))

(define (normalize-reply r)
  (cond ((or (eq? r #f) (null? r) (eq? r 'ok)) '(ok))
        ((string? r) (list 'inject r #f))
        ((and (pair? r)
              (symbol? (car r))
              (memq (car r) allowed-reply-heads)
              (wire-safe? r))
         r)
        (else '(err "handler returned an unsupported value"))))

(define (do-dispatch event plugin ctx)
  (let ((h (table-ref handlers-table (list plugin event) #f)))
    (if (not h)
        '(no-handler)
        (let ((r (try (lambda () (h ctx)))))
          (if (eq? (car r) 'err)
              (list 'err (cdr r))
              (normalize-reply (cdr r)))))))

;; ---------------------------------------------------------------------------
;; main loop
;; ---------------------------------------------------------------------------

(define (handle-msg m)
  (if (or (not (pair? m)) (not (symbol? (car m))))
      '(err "unsupported message")
      (let ((op (car m)) (args (cdr m)))
        (case op
          ((hello)
           (if (and (pair? args) (eqv? (car args) kernel-protocol-version))
               (list 'hello-ok kernel-protocol-version kernel-version)
               (list 'hello-err kernel-protocol-version kernel-version)))
          ((load-plugin)
           (if (and (= (length args) 2) (string? (car args)) (string? (cadr args)))
               (do-load-plugin (car args) (cadr args))
               '(err "load-plugin expects (load-plugin \"name\" \"source\")")))
          ((dispatch)
           (if (and (= (length args) 3)
                    (symbol? (car args))
                    (string? (cadr args))
                    (list? (caddr args)))
               (do-dispatch (car args) (cadr args) (caddr args))
               '(err "dispatch expects (dispatch event \"plugin\" ctx)")))
          ((redefine)
           (if (and (= (length args) 3)
                    (string? (car args))
                    (symbol? (cadr args))
                    (string? (caddr args)))
               (do-redefine (car args) (cadr args) (caddr args))
               '(err "redefine expects (redefine \"plugin\" event \"source\")")))
          ((inspect) (do-inspect))
          ((eval)
           (if (and (= (length args) 1) (string? (car args)))
               (do-eval (car args))
               '(err "eval expects (eval \"source\")")))
          ((list-commands) (do-list-commands))
          ((invoke-command)
           (if (and (= (length args) 3)
                    (string? (car args))
                    (string? (cadr args))
                    (string? (caddr args)))
               (do-invoke-command (car args) (cadr args) (caddr args))
               '(err "invoke-command expects (invoke-command \"plugin\" \"name\" \"args\")")))
          ((list-tools) (do-list-tools))
          ((invoke-tool)
           (if (and (= (length args) 3)
                    (string? (car args))
                    (string? (cadr args))
                    (string? (caddr args)))
               (do-invoke-tool (car args) (cadr args) (caddr args))
               '(err "invoke-tool expects (invoke-tool \"plugin\" \"name\" \"input-json\")")))
          ((quit) 'quit!)
          (else '(err "unknown op"))))))

(define (main-loop)
  (let loop ()
    (let ((frame (read-frame)))
      (if (not frame)
          (exit 0) ;; clean EOF: host is gone
          (let ((parsed (try (lambda () (with-input-from-string frame read)))))
            (if (eq? (car parsed) 'err)
                (begin
                  (write-frame (sexp->wire (list 'err "unreadable frame")))
                  (loop))
                (let ((reply (handle-msg (cdr parsed))))
                  (if (eq? reply 'quit!)
                      (begin
                        (write-frame (sexp->wire '(bye)))
                        (exit 0))
                      (begin
                        (write-frame (sexp->wire reply))
                        (loop))))))))))

(with-exception-catcher
 (lambda (e)
   (display (string-append "hyper-scheme-kernel uncaught: " (exception->message e))
            (current-error-port))
   (newline (current-error-port))
   (force-output (current-error-port))
   (exit 70))
 (lambda () (main-loop)))
