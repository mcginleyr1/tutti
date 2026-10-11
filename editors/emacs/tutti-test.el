;;; tutti-test.el --- Tests for tutti.el  -*- lexical-binding: t; -*-

;;; Commentary:

;; Against the stub editors/tests/tutti, which logs each call's argv:
;;
;;   emacs --batch -l ert -l <abs>/tutti.el -l <abs>/tutti-test.el \
;;     -f ert-run-tests-batch-and-exit

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'server)
(require 'tutti)

(defconst tutti-test--stub
  (expand-file-name "../tests/tutti" (file-name-directory (or load-file-name buffer-file-name))))

(defconst tutti-test--snapshot
  (concat "{\"type\":\"snapshot\",\"session\":\"tutti\",\"wire_rev\":9,\"attached_terminal\":null,\"panes\":["
          "{\"id\":3,\"workspace\":\"api\",\"dir\":\"/w/api\",\"tab\":\"main\",\"title\":\"claude\",\"agent\":\"claude\",\"state\":\"blocked\",\"source\":\"hook\",\"exited\":null,\"focused\":false},"
          "{\"id\":4,\"workspace\":\"web\",\"dir\":\"/w/web\",\"tab\":\"main\",\"title\":\"zsh\",\"agent\":null,\"state\":\"unknown\",\"source\":null,\"exited\":null,\"focused\":true},"
          "{\"id\":5,\"workspace\":\"web\",\"dir\":\"/w/web\",\"tab\":\"logs\",\"title\":\"codex\",\"agent\":\"codex\",\"state\":\"working\",\"source\":\"screen\",\"exited\":null,\"focused\":false}]}"))

(defun tutti-test--calls (answers thunk)
  "Call THUNK in pane 7 with the stub as `tutti-program'; the stub's calls.
ANSWERS gives the control files for a call by its number, as in
\((2 (\"out\" . \"text\") (\"exit\" . \"1\")))."
  (let* ((dir (make-temp-file "tutti-stub" t))
         (log (expand-file-name "log" dir))
         (process-environment (append (list (concat "TUTTI_STUB=" dir) "TUTTI_PANE=7")
                                      process-environment))
         (tutti-program tutti-test--stub))
    (unwind-protect
        (progn
          (pcase-dolist (`(,n . ,files) answers)
            (pcase-dolist (`(,name . ,content) files)
              (with-temp-file (format "%s/%d.%s" dir n name)
                (insert content))))
          (funcall thunk)
          (when (file-exists-p log)
            (with-temp-buffer
              (insert-file-contents log)
              (split-string (buffer-string) "\n" t))))
      (delete-directory dir t))))

(defun tutti-test--sent (thunk)
  "The strings THUNK sends to the terminal."
  (let (sent)
    (cl-letf (((symbol-function 'send-string-to-terminal)
               (lambda (string &rest _) (push string sent))))
      (funcall thunk))
    (nreverse sent)))

(defun tutti-test--settle (buffer)
  "Wait for BUFFER's `tutti watch' to end and its output to land."
  (let ((process (get-buffer-process buffer)))
    (while (or (process-live-p process) (accept-process-output process 0.05)))))

(defconst tutti-test--set
  "\e]1337;SetUserVar=TUTTI_KEYS=cGFuZQ==\a\e]1337;SetUserVar=TUTTI_EDITOR=ZW1hY3M=\a")
(defconst tutti-test--clear
  "\e]1337;SetUserVar=TUTTI_KEYS=\a\e]1337;SetUserVar=TUTTI_EDITOR=\a")

(ert-deftest tutti-mode-announces-clears-and-hooks ()
  (let ((process-environment (cons "TUTTI_PANE=7" process-environment)))
    (should (equal (tutti-test--sent
                    (lambda ()
                      (tutti-mode 1)
                      (should (memq #'tutti--clear kill-emacs-hook))
                      (run-hooks 'suspend-hook)
                      (run-hooks 'suspend-resume-hook)
                      (tutti-mode -1)
                      (should-not (memq #'tutti--clear kill-emacs-hook))
                      (should-not (memq #'tutti--clear suspend-hook))
                      (should-not (memq #'tutti--announce suspend-resume-hook))))
                   (list tutti-test--set tutti-test--clear tutti-test--set tutti-test--clear)))))

(ert-deftest tutti-mode-names-a-running-server-by-its-socket-path ()
  (let ((process-environment (cons "TUTTI_PANE=7" process-environment))
        (server-process t)
        (server-socket-dir "/tmp/emacs501"))
    (dolist (server-name '("work" "/tmp/emacs501/work"))
      (should (equal (tutti-test--sent #'tutti--announce)
                     '("\e]1337;SetUserVar=TUTTI_KEYS=cGFuZQ==\a\e]1337;SetUserVar=TUTTI_EDITOR=ZW1hY3MgL3RtcC9lbWFjczUwMS93b3Jr\a"))))))

(ert-deftest tutti-mode-is-inert-outside-a-pane-terminal ()
  (let ((process-environment (copy-sequence process-environment)))
    (setenv "TUTTI_PANE")
    (should-not (tutti-test--sent #'tutti--announce))
    (setenv "TUTTI_PANE" "7")
    (cl-letf (((symbol-function 'display-graphic-p) #'always))
      (should-not (tutti-test--sent #'tutti--announce)))))

(ert-deftest tutti-windmove-moves-within-then-past-the-edge ()
  (delete-other-windows)
  (let ((left (selected-window))
        (right (split-window-right)))
    (should (equal (tutti-test--calls
                    '((2 ("exit" . "1")))
                    (lambda ()
                      (tutti-windmove-right)
                      (should (eq (selected-window) right))
                      (tutti-windmove-right)
                      (tutti-windmove-up)
                      (tutti-windmove-left)
                      (should (eq (selected-window) left))
                      (tutti-windmove-down)))
                   '("[pane][focus][--direction][right][--from][7]"
                     "[pane][focus][--direction][up][--from][7]"
                     "[pane][focus][--direction][down][--from][7]"))))
  (delete-other-windows))

(ert-deftest tutti-windmove-never-signals ()
  (delete-other-windows)
  (let ((tutti-program "/nonexistent/tutti"))
    (tutti-windmove-left)))

(ert-deftest tutti-agents-lists-the-latest-snapshot-and-focuses ()
  (should (equal (tutti-test--calls
                  `((1 ("out" . ,(concat (replace-regexp-in-string "codex" "gemini" tutti-test--snapshot)
                                         "\n" tutti-test--snapshot "\n"))))
                  (lambda ()
                    (tutti-agents)
                    (tutti-test--settle "*tutti agents*")
                    (with-current-buffer "*tutti agents*"
                      (should (equal tabulated-list-entries
                                     '((3 ["blocked" "claude" "api"])
                                       (5 ["working" "codex" "web"]))))
                      (goto-char (point-min))
                      (while (not (eql (tabulated-list-get-id) 5))
                        (forward-line))
                      (should (eq (keymap-lookup tutti-agents-mode-map "RET") #'tutti-agents-focus))
                      (call-interactively #'tutti-agents-focus)
                      (kill-buffer))))
                 '("[watch]" "[pane][focus][5]"))))

(ert-deftest tutti-agents-refresh-restarts-the-watch ()
  (should (equal (tutti-test--calls
                  `((1 ("out" . ,(concat tutti-test--snapshot "\n")))
                    (2 ("out" . "{\"type\":\"snapshot\",\"panes\":[]}\n")))
                  (lambda ()
                    (tutti-agents)
                    (tutti-test--settle "*tutti agents*")
                    (with-current-buffer "*tutti agents*"
                      (revert-buffer)
                      (tutti-test--settle (current-buffer))
                      (should-not tabulated-list-entries)
                      (kill-buffer))))
                 '("[watch]" "[watch]"))))

(ert-deftest tutti-agents-reports-a-watch-that-fails ()
  (let (said)
    (tutti-test--calls
     '((1 ("err" . "tutti watch: no tutti-server is running for session 'tutti'") ("exit" . "1")))
     (lambda ()
       (cl-letf (((symbol-function 'message)
                  (lambda (string &rest args) (push (apply #'format-message string args) said))))
         (tutti-agents)
         (tutti-test--settle "*tutti agents*"))
       (kill-buffer "*tutti agents*")))
    (should (equal said '("tutti watch: no tutti-server is running for session 'tutti'")))))

(ert-deftest tutti-watch-filter-joins-partial-lines ()
  (with-temp-buffer
    (tutti-agents-mode)
    (let ((process (make-pipe-process :name "tutti-test" :buffer (current-buffer) :noquery t)))
      (unwind-protect
          (progn
            (tutti--watch-filter process (substring tutti-test--snapshot 0 100))
            (should-not tabulated-list-entries)
            (tutti--watch-filter process (concat (substring tutti-test--snapshot 100) "\n"))
            (should (equal (mapcar #'car tabulated-list-entries) '(3 5))))
        (delete-process process)))))

(ert-deftest tutti-agents-answer-is-a-guarded-send ()
  (let (shown)
    (should (equal (tutti-test--calls
                    '((1 ("out" . "9f3a0c1d2e4b5a6f\n"))
                      (2 ("out" . "Run cargo test?\n1. Yes\n2. No\n")))
                    (lambda ()
                      (cl-letf (((symbol-function 'read-string)
                                 (lambda (&rest _)
                                   (setq shown (list (get-buffer-window "*tutti prompt*")
                                                     (with-current-buffer "*tutti prompt*" (buffer-string))))
                                   "1")))
                        (tutti-agents-answer "3"))))
                   '("[pane][read][3][--fingerprint]"
                     "[pane][read][3][--lines][12]"
                     "[pane][send][3][--text][1][--expect][9f3a0c1d2e4b5a6f]")))
    (should (car shown))
    (should (equal (cadr shown) "Run cargo test?\n1. Yes\n2. No\n"))
    (should-not (get-buffer-window "*tutti prompt*"))))

(ert-deftest tutti-agents-answer-reports-a-refusal ()
  (tutti-test--calls
   '((1 ("out" . "9f3a0c1d2e4b5a6f\n"))
     (3 ("err" . "error: the prompt moved on; nothing was sent") ("exit" . "1")))
   (lambda ()
     (cl-letf (((symbol-function 'read-string) (lambda (&rest _) "1")))
       (should (equal (should-error (tutti-agents-answer "3") :type 'user-error)
                      '(user-error "error: the prompt moved on; nothing was sent")))))))

(defconst tutti-test--held
  (concat "{\"type\":\"panes\",\"panes\":["
          "{\"id\":3,\"title\":\"claude\",\"agent\":\"claude\",\"state\":\"blocked\",\"exited\":null,"
          "\"approval\":{\"id\":1,\"tool\":\"Bash\",\"summary\":\"rm -rf build\"}},"
          "{\"id\":4,\"title\":\"zsh\",\"agent\":null,\"state\":\"unknown\",\"exited\":null}]}\n"))

(ert-deftest tutti-agents-allow-and-deny-a-held-approval ()
  (let (asked)
    (should (equal (tutti-test--calls
                    `((1 ("out" . ,tutti-test--held))
                      (3 ("out" . ,tutti-test--held))
                      (5 ("out" . ,tutti-test--held)))
                    (lambda ()
                      (cl-letf (((symbol-function 'y-or-n-p) (lambda (prompt) (push prompt asked) t))
                                ((symbol-function 'tabulated-list-get-id) (lambda () 3))
                                ((symbol-function 'read-string)
                                 (let ((reasons '("not on main" "")))
                                   (lambda (prompt &rest _) (push prompt asked) (pop reasons)))))
                        (call-interactively #'tutti-agents-allow)
                        (call-interactively #'tutti-agents-deny)
                        (call-interactively #'tutti-agents-deny))))
                   '("[--json][pane][list]" "[pane][approve][3]"
                     "[--json][pane][list]" "[pane][approve][3][--deny][--message][not on main]"
                     "[--json][pane][list]" "[pane][approve][3][--deny]")))
    (should (equal (reverse asked)
                   '("Allow Bash · rm -rf build? "
                     "Deny Bash · rm -rf build, reason: "
                     "Deny Bash · rm -rf build, reason: ")))))

(ert-deftest tutti-agents-approval-needs-one-held ()
  (tutti-test--calls
   `((1 ("out" . ,tutti-test--held)))
   (lambda ()
     (should (equal (should-error (tutti-agents-allow "4") :type 'user-error)
                    '(user-error "Pane 4 holds no approval"))))))

(provide 'tutti-test)
;;; tutti-test.el ends here
