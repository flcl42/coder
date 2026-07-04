# coder

`coder` is a cross-platform detachable wrapper for `codex`.

It starts `codex` inside a background broker process that owns the pseudo-terminal.
The foreground `coder` process only attaches your current terminal to that broker.
If VS Code closes the terminal, the broker and Codex process keep running.

## Usage

```powershell
coder
coder resume
```

While attached, press `Ctrl+]` to detach without stopping Codex.
Multiple `coder` foreground processes can attach to the same running broker at
the same time. They share the same Codex pseudo-terminal; detaching one client
does not detach or interrupt the others.

The wrapper does not reserve public command-line arguments. Arguments are passed
to Codex. Plain `coder` and plain `coder resume` reattach to the default broker
when one exists. Targeted resumes such as `coder resume <session-id>` get their
own broker key, so separate resumed sessions do not steal each other's terminal.

Set `CODER_CODEX` to override the program used for Codex. If Codex cannot be
found, `coder` exits with a message explaining how to install Codex or set
`CODER_CODEX`.

Set `CODER_SESSION` to use a separate broker name:

```powershell
$env:CODER_SESSION = 'worktree-a'
coder
```

If Codex exits with an out-of-memory signature, the broker keeps the session
alive and restarts Codex after 3 minutes.

## Install

Release assets are unpacked single binaries. Linux is built with musl, Windows is
built with the static CRT, and macOS is shipped as a universal binary.

Linux, bash:

```bash
repo=flcl42/coder; curl -fsSL "https://github.com/$repo/releases/latest/download/coder-linux-x86_64" -o ./coder; chmod +x ./coder
```

macOS, zsh:

```zsh
repo=flcl42/coder; curl -fsSL "https://github.com/$repo/releases/latest/download/coder-macos-universal" -o ./coder; chmod +x ./coder
```

Windows, PowerShell:

```powershell
$repo='flcl42/coder'; Invoke-WebRequest "https://github.com/$repo/releases/latest/download/coder-windows-x86_64.exe" -OutFile ".\coder.exe"
```

## Development

When replacing a local `coder.exe`, stop any running broker/client processes
first if they lock the file, then replace the binary directly. Do not leave a
separate fixed-name executable as the normal test path.

Exception: for input-handling fixes or other changes where preserving active
sessions is more important than immediate install, do not stop live `coder.exe`
processes. Build `target\release\coder.exe` and install it after the active
sessions exit.
