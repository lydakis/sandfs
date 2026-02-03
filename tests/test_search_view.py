from sandfs import SandboxShell, VirtualFileSystem


def test_search_command():
    vfs = VirtualFileSystem()
    vfs.write_file("/workspace/README.md", "hello world\n")
    shell = SandboxShell(vfs)
    res = shell.exec("search hello /workspace")
    assert "/workspace/README.md" in res.stdout


def test_search_view_tree_and_content():
    vfs = VirtualFileSystem()
    vfs.write_file("/workspace/README.md", "hello world\n")
    vfs.enable_search_view()
    shell = SandboxShell(vfs)

    res = shell.exec("ls /@search?q=hello")
    assert "workspace" in res.stdout

    res2 = shell.exec("cat /@search?q=hello/workspace/README.md")
    assert "/workspace/README.md:1:hello world" in res2.stdout


def test_search_view_query_params_with_path_prefix():
    vfs = VirtualFileSystem()
    vfs.write_file("/workspace/README.md", "Hello World\n")
    vfs.enable_search_view()
    shell = SandboxShell(vfs)

    res = shell.exec("ls /@search?q=hello&ignore_case=1&path=/workspace")
    assert "workspace" in res.stdout
