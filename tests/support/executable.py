"""PATH test doubles share one executable inode to avoid cold script launches.

Harness-owned scripts should instead be invoked through their interpreter.
Payloads see their .script path as $0 or sys.argv[0], not the command symlink.
"""

import os
import shutil
import sys
from pathlib import Path

IS_WINDOWS = os.name == "nt"

SCRIPT_RUNNER = Path(__file__).with_name("script-runner")

# Interpreters that are shells, which this machine keeps somewhere of its own.
SHELLS = ("sh", "bash", "dash", "zsh", "ksh")


def install_script(path: Path, body: str) -> Path:
    """Install a shared entry point and a non-executable interpreter payload.

    The real command name remains on PATH, so production command discovery and
    subprocess execution are exercised. Only the test double's launch changes.

    Windows has no shebang to execute and no `sh` of its own on PATH, so the
    entry point there is a `.cmd` that hands the payload to the interpreter its
    shebang names -- for a shell, the one that came with Git, which workmux
    needs anyway. The payload is the same one the POSIX runner would read.
    """
    if not body.startswith("#!"):
        raise ValueError("Test scripts require an explicit interpreter")
    payload = path.with_name(path.name + ".script")
    payload.write_text(body)

    if IS_WINDOWS:
        path.write_text(posix_entry_point(body, payload))
        path.with_name(path.name + ".cmd").write_text(entry_point(body, payload))
        return path

    if path.exists() or path.is_symlink():
        path.unlink()
    path.symlink_to(SCRIPT_RUNNER)
    return path


def entry_point(body: str, payload: Path) -> str:
    """The `.cmd` that runs `payload` with the interpreter its shebang names."""
    words = [*interpreter(body.splitlines()[0]), str(payload), "%*"]
    command = " ".join(f'"{word}"' if " " in word else word for word in words)
    return f"@echo off\n{command}\n"


def posix_entry_point(body: str, payload: Path) -> str:
    """The entry point a POSIX shell on Windows runs: the name, with no suffix.

    Windows starts an image, so a cmd or PowerShell pane needs the `.cmd`
    beside this file; a shell from a Git installation starts a script whose
    first line is a shebang, whatever the file is called, and it finds this one
    by the name the test asked for. Both run the same payload through the same
    interpreter -- the shim spells the paths the POSIX way, which is the one
    spelling both this shell and the interpreter read.
    """
    words = [*interpreter(body.splitlines()[0]), str(payload)]
    command = " ".join(as_posix_path(word) for word in words)
    return f'#!/bin/sh\nexec {command} "$@"\n'


def as_posix_path(path: str) -> str:
    """`path` the way the shell that runs it spells it: forward slashes, quoted."""
    return f'"{path.replace(chr(92), "/")}"'


def interpreter(shebang: str) -> list[str]:
    """The command that runs a payload, from the interpreter its shebang names.

    `#!/usr/bin/env NAME` and `#!/path/to/NAME` both name NAME. This machine
    knows no `/usr/bin`, so either spelling of Python becomes the interpreter
    running this suite, and a shell becomes the shell Git was installed with.
    """
    words = shebang.removeprefix("#!").split()
    if not words:
        raise ValueError(f"Test scripts require an explicit interpreter: {shebang!r}")
    program = words[1] if words[0].endswith("env") and len(words) > 1 else words[0]
    name = program.replace("\\", "/").rsplit("/", 1)[-1]

    if name.lower().startswith("python"):
        return [sys.executable]
    if name in SHELLS:
        return [shell(name)]
    return [program]


def shell(name: str) -> str:
    """A shell from the install that brought Git here.

    Git for Windows is the one thing a Windows host is sure to have that also
    carries a POSIX shell, and asking Git where its own binaries are keeps this
    from depending on a PATH the tests do not control.
    """
    git = shutil.which("git")
    root = Path(git).resolve().parent.parent if git else None
    candidates = (
        [root / "bin" / f"{name}.exe", root / "usr" / "bin" / f"{name}.exe"]
        if root
        else []
    )
    for candidate in candidates:
        if candidate.exists():
            return str(candidate)
    raise RuntimeError(
        f"No {name} on PATH and none beside git; install Git for Windows to run "
        "the test doubles that are written as shell scripts"
    )
