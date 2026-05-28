# coder

`coder.exe` is a detachable wrapper for `codex`.

It starts `codex` inside a background broker process that owns the pseudo-terminal.
The foreground `coder` process only attaches your current terminal to that broker.
If VS Code closes the terminal, the broker and Codex process keep running.

## Usage

```powershell
coder
coder resume
```

While attached, press `Ctrl+]` to detach without stopping Codex.

The wrapper does not reserve public command-line arguments. Arguments are passed
to Codex; the only extra behavior is that `coder` and `coder resume` reattach to
an already-running broker when one exists.

Set `CODER_CODEX` to override the program used for Codex. By default this machine
uses `C:\Programs\nodejs\node.exe` with the installed Codex JavaScript entrypoint.

Set `CODER_SESSION` to use a separate broker name:

```powershell
$env:CODER_SESSION = 'worktree-a'
coder
```

If Codex exits with an out-of-memory signature, the broker keeps the session
alive and restarts Codex after 3 minutes.
