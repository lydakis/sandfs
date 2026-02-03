import builtins

import pytest

from sandfs.cli import main


def test_cli_exec_outputs(capsys):
    with pytest.raises(SystemExit) as exc:
        main(["exec", "echo hi"])
    assert exc.value.code == 0
    captured = capsys.readouterr()
    assert "hi" in captured.out


def test_cli_shell_repl(monkeypatch, capsys):
    inputs = iter(["echo hello", ":q"])

    def fake_input(_: str) -> str:
        return next(inputs)

    monkeypatch.setattr(builtins, "input", fake_input)
    with pytest.raises(SystemExit) as exc:
        main(["shell"])
    assert exc.value.code == 0
    captured = capsys.readouterr()
    assert "hello" in captured.out
