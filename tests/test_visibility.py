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


def test_view_path_prefix_filters():
    vfs = VirtualFileSystem()
    vfs.write_file("/blue/allowed.txt", "ok")
    vfs.write_file("/workspace/hidden.txt", "nope")
    view = VisibilityView(path_prefixes={"/blue"})
    shell = SandboxShell(vfs, view=view)
    listing = shell.exec("ls /").stdout
    assert "blue" in listing
    assert "workspace" not in listing


def test_view_metadata_filters():
    vfs = VirtualFileSystem()
    vfs.write_file("/blue/keep.txt", "keep")
    vfs.write_file("/blue/drop.txt", "drop")
    keep_node = vfs.get_node("/blue/keep.txt")
    drop_node = vfs.get_node("/blue/drop.txt")
    keep_node.metadata["tag"] = "keep"
    drop_node.metadata["tag"] = "drop"

    view = VisibilityView(metadata_filters={"tag": "keep"})
    shell = SandboxShell(vfs, view=view)
    listing = shell.exec("ls /blue").stdout
    assert "keep.txt" in listing
    assert "drop.txt" not in listing


def test_metadata_filters_do_not_bypass_classification_for_dirs():
    vfs = VirtualFileSystem()
    vfs.mkdir("/secret")
    vfs.write_file("/secret/file.txt", "hidden")
    vfs.set_policy("/secret", NodePolicy(classification="secret"))

    view = VisibilityView(classifications={"public"}, metadata_filters={"tag": "keep"})
    shell = SandboxShell(vfs, view=view)
    res = shell.exec("ls /secret")
    assert res.exit_code == 1
    assert "hidden" in res.stderr.lower()
