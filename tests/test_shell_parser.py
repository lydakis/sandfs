import pytest

from sandfs.shell_parser import parse_pipeline


def test_parse_pipeline_empty_returns_no_commands():
    pipeline = parse_pipeline("\n")
    assert pipeline.commands == []


def test_parse_pipeline_assignment_only():
    pipeline = parse_pipeline("FOO=bar BAR=baz")
    assert len(pipeline.commands) == 1
    command = pipeline.commands[0]
    assert command.name is None
    assert command.assignments == {"FOO": "bar", "BAR": "baz"}


def test_parse_pipeline_missing_redirection_target():
    with pytest.raises(ValueError):
        parse_pipeline("echo hi >")


def test_parse_pipeline_missing_command_before_pipe():
    with pytest.raises(ValueError):
        parse_pipeline("echo hi |")


def test_parse_pipeline_redirection_without_command():
    with pytest.raises(ValueError):
        parse_pipeline("> out.txt")
