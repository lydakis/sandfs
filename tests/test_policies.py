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


def test_read_only_directory_blocks_new_files():
    vfs = VirtualFileSystem()
    vfs.mkdir("/ro")
    vfs.set_policy("/ro", NodePolicy(writable=False))

    with pytest.raises(InvalidOperation):
        vfs.write_file("/ro/new.txt", "x")
    with pytest.raises(InvalidOperation):
        vfs.write_file("/ro/nested/new.txt", "x")
    with pytest.raises(InvalidOperation):
        vfs.touch("/ro/touched.txt")

    assert not vfs.exists("/ro/new.txt")
    assert not vfs.exists("/ro/nested")
    assert not vfs.exists("/ro/touched.txt")


def test_host_does_not_export_hidden_or_unreadable_files():
    vfs = VirtualFileSystem()
    vfs.write_file("/work/a.txt", "a")
    vfs.write_file("/secret/key.txt", "TOPSECRET")
    vfs.write_file("/work/private.txt", "PRIVATE")
    vfs.set_policy("/secret/key.txt", NodePolicy(readable=False))
    vfs.set_policy("/work/private.txt", NodePolicy(classification="private"))
    shell = SandboxShell(vfs, view=VisibilityView(classifications={"public"}), host_commands=True)

    leaked = shell.exec("host -p /work cat /secret/key.txt")
    listing = shell.exec("host -p /work ls")

    assert "TOPSECRET" not in leaked.stdout
    assert leaked.exit_code != 0
    assert listing.stdout.split() == ["a.txt"]


def test_host_sync_keeps_files_it_did_not_export():
    vfs = VirtualFileSystem()
    vfs.write_file("/work/a.txt", "a")
    vfs.write_file("/work/private.txt", "PRIVATE")
    vfs.set_policy("/work/private.txt", NodePolicy(classification="private"))
    shell = SandboxShell(vfs, view=VisibilityView(classifications={"public"}), host_commands=True)

    result = shell.exec("host -p /work touch b.txt")

    assert result.exit_code == 0
    assert vfs.read_file("/work/private.txt") == "PRIVATE"
    assert vfs.exists("/work/b.txt")


def test_host_sync_rejects_writes_beneath_hidden_directories():
    vfs = VirtualFileSystem()
    vfs.write_file("/secret/key.txt", "TOPSECRET")
    vfs.write_file("/work/a.txt", "a")
    vfs.set_policy("/secret", NodePolicy(readable=False))
    shell = SandboxShell(vfs, host_commands=True)

    result = shell.exec("host -p / sh -c 'mkdir secret; echo hacked > secret/new.txt'")

    assert not vfs.exists("/secret/new.txt")
    assert "did not sync /secret" in result.stderr


def test_host_sync_keeps_hidden_children_of_deleted_directories():
    vfs = VirtualFileSystem()
    vfs.write_file("/work/a.txt", "a")
    vfs.write_file("/work/private/notes.txt", "PRIVATE")
    vfs.set_policy("/work/private", NodePolicy(readable=False))
    shell = SandboxShell(vfs, host_commands=True)

    shell.exec("host -p / rm -rf work")

    assert not vfs.exists("/work/a.txt")
    assert vfs.exists("/work/private/notes.txt")


def test_export_skips_an_unreadable_root(tmp_path):
    vfs = VirtualFileSystem()
    vfs.write_file("/secret/key.txt", "TOPSECRET")
    vfs.set_policy("/secret", NodePolicy(readable=False))
    vfs.set_policy("/secret/key.txt", NodePolicy(readable=True))

    with vfs.materialize("/secret") as root:
        assert list(root.iterdir()) == []
