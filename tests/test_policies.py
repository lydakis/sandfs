import pytest

from sandfs import SandboxShell, VirtualFileSystem
from sandfs.exceptions import InvalidOperation
from sandfs.policies import NodePolicy, VisibilityView


def test_readonly_policy_blocks_write():
    vfs = VirtualFileSystem()
    vfs.write_file("/blue/identity/persona.md", "persona")
    vfs.set_policy("/blue/identity/persona.md", NodePolicy(readable=True, writable=False))
    with pytest.raises(InvalidOperation):
        vfs.write_file("/blue/identity/persona.md", "update")


def test_append_only_policy_allows_appends():
    vfs = VirtualFileSystem()
    vfs.write_file("/logs/run.txt", "start\n")
    vfs.set_policy("/logs/run.txt", NodePolicy(append_only=True))
    with pytest.raises(InvalidOperation):
        vfs.write_file("/logs/run.txt", "reset\n")
    vfs.append_file("/logs/run.txt", "next\n")
    assert "next" in vfs.read_file("/logs/run.txt")


def test_visibility_view_hides_nodes_from_shell():
    vfs = VirtualFileSystem()
    vfs.write_file("/blue/public.txt", "public")
    vfs.write_file("/blue/private.txt", "secret")
    vfs.set_policy("/blue/private.txt", NodePolicy(classification="private"))
    shell = SandboxShell(vfs, view=VisibilityView(classifications={"public"}))
    listing = shell.exec("ls /blue").stdout
    assert "public.txt" in listing
    assert "private.txt" not in listing
    hidden = shell.exec("cat /blue/private.txt")
    assert hidden.exit_code == 1
    assert "hidden" in hidden.stderr.lower()
