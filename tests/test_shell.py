from sandfs import SandboxShell, VirtualFileSystem


def setup_shell() -> SandboxShell:
    vfs = VirtualFileSystem()
    vfs.write_file("/workspace/app.py", "print('hi')\n")
    vfs.write_file("/workspace/README.md", "hello world\n")
    return SandboxShell(vfs)


def test_ls_and_cd():
    shell = setup_shell()
    result = shell.exec("ls /workspace")
    assert "app.py" in result.stdout
    shell.exec("cd /workspace")
    assert shell.exec("pwd").stdout.endswith("/workspace")


def test_cat_and_write():
    shell = setup_shell()
    shell.exec("write /workspace/app.py --append print('bye')")
    out = shell.exec("cat /workspace/app.py").stdout
    assert "bye" in out


def test_rg_search():
    shell = setup_shell()
    res = shell.exec("rg hello /workspace")
    assert "/workspace/README.md:" in res.stdout


def test_python_executor():
    shell = setup_shell()
    res = shell.exec('python -c "print(len(vfs.ls(\'/workspace\')))"')
    assert res.stdout.strip().isdigit()


def test_host_command_grep():
    shell = setup_shell()
    res = shell.exec("host -p /workspace grep hello README.md")
    assert "hello world" in res.stdout
    assert res.exit_code == 0


def test_host_command_requires_subcommand():
    shell = setup_shell()
    res = shell.exec("host -p /workspace")
    assert res.exit_code != 0
    assert "expects a command" in res.stderr.lower()


def test_agent_mode_blocks_commands():
    vfs = VirtualFileSystem()
    vfs.write_file("/workspace/allowed.txt", "ok")
    shell = SandboxShell(vfs, allowed_commands={"ls", "cat"})
    res = shell.exec("host -p /workspace ls")
    assert res.exit_code == 1
    assert "disabled" in res.stderr.lower()


def test_agent_mode_output_limit():
    vfs = VirtualFileSystem()
    vfs.write_file("/workspace/big.txt", "0123456789")
    shell = SandboxShell(vfs, max_output_bytes=5)
    res = shell.exec("cat /workspace/big.txt")
    assert res.exit_code == 1
    assert "output limit" in res.stderr.lower()
