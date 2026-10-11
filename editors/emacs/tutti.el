;;; tutti.el --- Emacs inside a tutti pane  -*- lexical-binding: t; -*-

;; Author: Rob McGinley <mcginleyr1@gmail.com>
;; Version: 0.1.0
;; Package-Requires: ((emacs "29.1"))
;; Keywords: terminals, processes, tools

;;; Commentary:

;; For Emacs running in a terminal inside a tutti pane ($TUTTI_PANE set):
;;
;; - `tutti-mode' tells tutti that Emacs runs here, so tutti hands its direct
;;   keys (Ctrl+h/j/k/l, Alt+h/j/k/l, Alt+x) to Emacs and `tutti edit' opens
;;   files in this Emacs.
;; - `tutti-windmove-left' and friends move between windows, and past the
;;   frame's edge on to tutti's panes.
;; - `tutti-agents' lists the session's agents, kept live by `tutti watch':
;;   RET focuses one, p answers a blocked one, y/n answer a held approval.

;;; Code:

(require 'seq)
(require 'tabulated-list)
(require 'windmove)

(defvar server-name)
(defvar server-process)
(defvar server-socket-dir)

(defgroup tutti nil
  "Emacs inside a tutti pane."
  :group 'tools
  :prefix "tutti-")

(defcustom tutti-program "tutti"
  "The tutti executable."
  :type 'string)

;;;; Announcing Emacs to tutti

(defun tutti--user-var (name value)
  "The OSC 1337 sequence setting the user var NAME to VALUE; empty clears it."
  (format "\e]1337;SetUserVar=%s=%s\a"
          name (base64-encode-string (encode-coding-string value 'utf-8) t)))

(defun tutti--send-vars (keys editor)
  "Set tutti's TUTTI_KEYS to KEYS and TUTTI_EDITOR to EDITOR.
Only a terminal frame in a tutti pane has a tutti to tell."
  (when (and (getenv "TUTTI_PANE") (not (display-graphic-p)))
    (send-string-to-terminal
     (concat (tutti--user-var "TUTTI_KEYS" keys)
             (tutti--user-var "TUTTI_EDITOR" editor)))))

(defun tutti--announce ()
  "Tell tutti this Emacs takes its keys and opens its files.
A running server is named by its socket's absolute path: an emacsclient
started with another environment would look for a bare name elsewhere."
  (tutti--send-vars "pane" (if (bound-and-true-p server-process)
                               (concat "emacs " (expand-file-name server-name server-socket-dir))
                             "emacs")))

(defun tutti--clear ()
  "Take back what `tutti--announce' told tutti."
  (tutti--send-vars "" ""))

;;;###autoload
(define-minor-mode tutti-mode
  "Tell tutti that Emacs runs in its pane.
While on, tutti passes its direct keys (Ctrl+h/j/k/l, Alt+h/j/k/l,
Alt+x) to Emacs, and `tutti edit' opens files here: through this
Emacs's server when one runs, so turn the mode on after
`server-start'.  Cleared while Emacs is suspended and when it exits.
Does nothing outside a terminal frame in a tutti pane."
  :global t
  :group 'tutti
  (let ((hook (if tutti-mode #'add-hook #'remove-hook)))
    (funcall hook 'kill-emacs-hook #'tutti--clear)
    (funcall hook 'suspend-hook #'tutti--clear)
    (funcall hook 'suspend-resume-hook #'tutti--announce))
  (if tutti-mode (tutti--announce) (tutti--clear)))

;;;; Moving between windows and panes

(defun tutti--windmove (direction)
  "Select the window in DIRECTION, or past the frame's edge, tutti's pane.
The edge is wherever windmove refuses: no window there, or only the
inactive minibuffer.  Outside a tutti pane, windmove's refusal stands."
  (condition-case refusal
      (windmove-do-window-select direction)
    (error
     (let ((pane (or (getenv "TUTTI_PANE") (signal (car refusal) (cdr refusal)))))
       (ignore-errors
         (call-process tutti-program nil nil nil "pane" "focus"
                       "--direction" (symbol-name direction) "--from" pane))))))

;;;###autoload
(defun tutti-windmove-left ()
  "Select the window to the left, or past the frame's edge, tutti's pane."
  (interactive)
  (tutti--windmove 'left))

;;;###autoload
(defun tutti-windmove-right ()
  "Select the window to the right, or past the frame's edge, tutti's pane."
  (interactive)
  (tutti--windmove 'right))

;;;###autoload
(defun tutti-windmove-up ()
  "Select the window above, or past the frame's edge, tutti's pane."
  (interactive)
  (tutti--windmove 'up))

;;;###autoload
(defun tutti-windmove-down ()
  "Select the window below, or past the frame's edge, tutti's pane."
  (interactive)
  (tutti--windmove 'down))

;;;; The agents list

(defun tutti--run (&rest args)
  "Run `tutti-program' with ARGS and return its output.
A failure is a `user-error' carrying what tutti said."
  (with-temp-buffer
    (unless (eql 0 (apply #'call-process tutti-program nil t nil args))
      (user-error "%s" (string-trim (buffer-string))))
    (buffer-string)))

(defun tutti--json (string)
  "STRING parsed as JSON, objects as alists and null as nil."
  (json-parse-string string :object-type 'alist :null-object nil))

(defun tutti--entry (pane)
  "PANE, an object from a `tutti watch' snapshot, as a `tabulated-list' entry."
  (let-alist pane
    (list .id (vector (propertize .state 'face (pcase .state
                                                ("blocked" 'error)
                                                ("working" 'warning)
                                                ("done" 'success)
                                                (_ 'shadow)))
                      .title .workspace))))

(defun tutti--watch-filter (process output)
  "Show each full snapshot PROCESS prints in OUTPUT in its buffer.
Lines that are not JSON are what tutti said; the sentinel reports
the last."
  (let ((lines (split-string (concat (process-get process 'partial) output) "\n")))
    (process-put process 'partial (car (last lines)))
    (dolist (line (butlast lines))
      (let ((snapshot (condition-case nil (tutti--json line) (json-parse-error nil))))
        (cond ((string-empty-p line))
              ((not snapshot) (process-put process 'said line))
              ((buffer-live-p (process-buffer process))
               (with-current-buffer (process-buffer process)
                 (setq tabulated-list-entries
                       (mapcar #'tutti--entry
                               (seq-filter (lambda (pane) (alist-get 'agent pane))
                                           (alist-get 'panes snapshot))))
                 (tabulated-list-print t))))))))

(defun tutti--watch-sentinel (process event)
  "Report that PROCESS, a `tutti watch', ended, as EVENT says."
  (when (eq (process-status process) 'exit)
    (tutti--watch-filter process "\n")
    (message "%s" (or (process-get process 'said)
                      (concat "tutti watch: " (string-trim event))))))

(defun tutti--watch ()
  "Start a `tutti watch' feeding the current buffer, ending any before it."
  (when (get-buffer-process (current-buffer))
    (delete-process (get-buffer-process (current-buffer))))
  (make-process :name "tutti-watch"
                :command (list tutti-program "watch")
                :buffer (current-buffer)
                :connection-type 'pipe
                :noquery t
                :filter #'tutti--watch-filter
                :sentinel #'tutti--watch-sentinel))

(defun tutti--id ()
  "The pane id of the agent on this line, as a string."
  (number-to-string (or (tabulated-list-get-id) (user-error "No agent here"))))

(defun tutti-agents-focus (id)
  "Focus pane ID, the agent on this line, in tutti."
  (interactive (list (tutti--id)) tutti-agents-mode)
  (tutti--run "pane" "focus" id))

(defun tutti-agents-answer (id)
  "Answer the blocked agent in pane ID with tutti's guarded send.
Takes the prompt's fingerprint first, then shows the screen it covers
and reads the answer, so a prompt that moved on in between refuses it.
The answer is sent as typed; \\[quoted-insert] RET adds an Enter."
  (interactive (list (tutti--id)) tutti-agents-mode)
  (let* ((fingerprint (string-trim (tutti--run "pane" "read" id "--fingerprint")))
         (screen (tutti--run "pane" "read" id "--lines" "12"))
         (window (with-current-buffer (get-buffer-create "*tutti prompt*")
                   (let ((inhibit-read-only t))
                     (erase-buffer)
                     (insert screen))
                   (special-mode)
                   (display-buffer (current-buffer)
                                   '(display-buffer-in-side-window (side . bottom))))))
    (unwind-protect
        (let ((answer (read-string (format "Answer pane %s: " id))))
          (tutti--run "pane" "send" id "--text" answer "--expect" fingerprint)
          (message "Answered pane %s" id))
      (when (window-live-p window)
        (delete-window window)))))

(defun tutti--approval (id)
  "The approval pane ID holds, from `tutti --json pane list'."
  (or (alist-get 'approval
                 (seq-find (lambda (pane) (equal (number-to-string (alist-get 'id pane)) id))
                           (alist-get 'panes (tutti--json (tutti--run "--json" "pane" "list")))))
      (user-error "Pane %s holds no approval" id)))

(defun tutti-agents-allow (id)
  "Allow the tool call the held approval in pane ID asks for."
  (interactive (list (tutti--id)) tutti-agents-mode)
  (let-alist (tutti--approval id)
    (when (y-or-n-p (format "Allow %s · %s? " .tool .summary))
      (tutti--run "pane" "approve" id)
      (message "Allowed %s" .tool))))

(defun tutti-agents-deny (id reason)
  "Deny the tool call the held approval in pane ID asks for.
REASON is what the agent is told; empty, tutti's default."
  (interactive (let ((id (tutti--id)))
                 (let-alist (tutti--approval id)
                   (list id (read-string (format "Deny %s · %s, reason: " .tool .summary)))))
               tutti-agents-mode)
  (apply #'tutti--run "pane" "approve" id "--deny"
         (unless (string-empty-p reason) (list "--message" reason)))
  (message "Denied"))

(defvar-keymap tutti-agents-mode-map
  :parent tabulated-list-mode-map
  "RET" #'tutti-agents-focus
  "p" #'tutti-agents-answer
  "y" #'tutti-agents-allow
  "n" #'tutti-agents-deny)

(define-derived-mode tutti-agents-mode tabulated-list-mode "tutti agents"
  "The tutti session's agents, kept live by `tutti watch'.
\\{tutti-agents-mode-map}"
  (setq tabulated-list-format [("State" 9 t) ("Title" 30 t) ("Workspace" 0 t)])
  (add-hook 'tabulated-list-revert-hook #'tutti--watch nil t)
  (tabulated-list-init-header))

;;;###autoload
(defun tutti-agents ()
  "List the tutti session's agents, kept live by `tutti watch'."
  (interactive)
  (with-current-buffer (get-buffer-create "*tutti agents*")
    (unless (derived-mode-p 'tutti-agents-mode)
      (tutti-agents-mode))
    (unless (get-buffer-process (current-buffer))
      (tutti--watch))
    (pop-to-buffer (current-buffer))))

(provide 'tutti)
;;; tutti.el ends here
