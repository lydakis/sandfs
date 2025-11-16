from sandfs import NodePolicy, SandboxShell, VirtualFileSystem, VisibilityView


def test_contact_visibility_filters_shell():
    vfs = VirtualFileSystem()
    vfs.write_file("/blue/public.txt", "pub")
    vfs.write_file("/blue/bob.txt", "bob")
    vfs.write_file("/blue/alice.txt", "alice")

    vfs.set_policy("/blue/bob.txt", NodePolicy(classification="private", principals={"bob"}))
    vfs.set_policy("/blue/alice.txt", NodePolicy(classification="private", principals={"alice"}))

    shell = SandboxShell(vfs, view=VisibilityView(classifications={"public"}, principals={"bob"}))
    listing = shell.exec("ls /blue").stdout
    assert "public.txt" in listing
    assert "bob.txt" in listing
    assert "alice.txt" not in listing

    res = shell.exec("cat /blue/alice.txt")
    assert res.exit_code == 1
    assert "hidden" in res.stderr.lower()


def test_principal_only_file_hidden_without_principal_view():
    vfs = VirtualFileSystem()
    vfs.write_file("/blue/public.txt", "pub")
    vfs.set_policy(
        "/blue/public.txt",
        NodePolicy(classification="public", principals={"alice"}),
    )

    view = VisibilityView(classifications={"public"})
    assert view.principals is None
    assert not view.allows(vfs.get_policy("/blue/public.txt"))

    shell = SandboxShell(vfs, view=view)
    listing = shell.exec("ls /blue").stdout
    assert "public.txt" not in listing

    res = shell.exec("cat /blue/public.txt")
    assert res.exit_code == 1
    assert "hidden" in res.stderr.lower()
