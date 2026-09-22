"""A real agent, in a real worktree, on the backend under test.

The rest of the suite runs fake agents: it has to, or every run would need a
model, a login, and minutes. This file is the one check that the whole path
-- worktree, tab, prompt injection, agent input protocol -- carries a real
one, so it runs only when it is asked for:

    WORKMUX_REAL_AGENT=codex pytest tests/test_real_agent.py --backend wezterm

The agent is started with the prompt workmux injects and has to answer with
the word the prompt asks for. Nothing about the answer is a model test; the
interesting failures are a tab that never starts the agent, an agent that
never sees the prompt, or a prompt delivered as keystrokes the agent's input
protocol does not speak.
"""

import os
import time

import pytest

from .conftest import (
    get_window_name,
    get_worktree_path,
    pane_quote,
    run_workmux_command,
    write_workmux_config,
)

AGENT = os.environ.get("WORKMUX_REAL_AGENT")

pytestmark = pytest.mark.skipif(
    not AGENT,
    reason="set WORKMUX_REAL_AGENT=<agent> to run this against a real CLI",
)

ANSWER = "PONG"
PROMPT = f"Reply with exactly the word {ANSWER} and nothing else."
ANSWER_TIMEOUT = 300.0


def test_real_agent_answers_the_injected_prompt(
    mux_server, workmux_exe_path, mux_repo_path
):
    env = mux_server
    write_workmux_config(
        mux_repo_path,
        panes=[{"command": AGENT}],
        agent=AGENT,
        env=env,
    )

    run_workmux_command(
        env,
        workmux_exe_path,
        mux_repo_path,
        f"add real-agent -p {pane_quote(PROMPT)}",
        timeout=90,
    )

    window = get_window_name("real-agent")
    assert get_worktree_path(mux_repo_path, "real-agent").exists()

    # Codex asks a new directory to be trusted before it will read the prompt
    # that is already sitting in its composer. One Enter answers it, once.
    answered_onboarding = False
    deadline = time.monotonic() + ANSWER_TIMEOUT
    content = ""
    while time.monotonic() < deadline:
        content = env.capture_pane(window) or ""
        if ANSWER in content:
            return
        if not answered_onboarding and "trust the contents" in content:
            env.send_keys(window, "", enter=True)
            answered_onboarding = True
        time.sleep(3)

    pytest.fail(
        f"{AGENT} did not answer {ANSWER!r} within {ANSWER_TIMEOUT:.0f}s.\n"
        f"--- FINAL PANE CONTENT ---\n{content}\n--------------------------"
    )
