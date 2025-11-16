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


def test_python3_alias():
    shell = setup_shell()
    res = shell.exec('python3 -c "print(1+1)"')
    assert res.stdout.strip() == "2"


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


def test_unknown_command_falls_back_to_host():
    shell = setup_shell()
    res = shell.exec("echo hello from host")
    assert "hello from host" in res.stdout


def test_bash_is_routed_through_host():
    shell = setup_shell()
    res = shell.exec("bash -lc 'printf test'")
    assert res.stdout.strip() == "test"


def test_python3_allowed_when_python_disallowed():
    base_shell = setup_shell()
    shell = SandboxShell(
        base_shell.vfs,
        allowed_commands={"ls", "cat", "python3", "host", "bash", "sh", "help"},
    )
    res = shell.exec('python3 -c "print(5)"')
    assert res.stdout.strip() == "5"


def test_host_fallback_translates_executable_path():
    shell = setup_shell()
    shell.vfs.write_file("/workspace/run.sh", "#!/bin/sh\necho script works\n")
    res = shell.exec("/workspace/run.sh")
    assert "Permission" in res.stderr or "denied" in res.stderr.lower()


def test_host_relative_path_option():
    shell = setup_shell()
    res = shell.exec("host -p ./workspace ls")
    assert "README.md" in res.stdout


def test_help_lists_commands():
    shell = setup_shell()
    res = shell.exec("help")
    assert "ls - List directory contents" in res.stdout


def test_append_command():
    shell = setup_shell()
    shell.exec("append /workspace/README.md appended text")
    out = shell.exec("cat /workspace/README.md").stdout
    assert "appended text" in out


def test_ls_accepts_flags():
    shell = setup_shell()
    res = shell.exec("ls -la")
    assert "workspace" in res.stdout


def test_ls_on_blue_directory_via_host():
    shell = setup_shell()
    shell.exec("mkdir /blue")
    shell.exec("write /blue/file.txt hello")
    res = shell.exec("ls /blue")
    assert "file.txt" in res.stdout


def test_heredoc_write_via_bash():
    shell = setup_shell()
    shell.exec("mkdir /blue")
    cmd = "bash -lc 'printf \"hello from heredoc\" > /blue/note.txt'"
    shell.exec(cmd)
    assert "hello from heredoc" in shell.exec("cat /blue/note.txt").stdout


def test_host_rm_syncs_back():
    shell = setup_shell()
    assert shell.exec("host -p /workspace rm app.py").exit_code == 0
    assert not shell.vfs.exists("/workspace/app.py")


def test_host_sync_skips_read_only_files():
    shell = setup_shell()
    node = shell.vfs._resolve_node("/workspace/app.py")
    node.policy.writable = False

    result = shell.exec("host -p /workspace ls")

    assert result.exit_code == 0


def test_host_sync_does_not_rewrite_unchanged_files():
    shell = setup_shell()
    original_version = shell.vfs.get_version("/workspace/app.py")

    result = shell.exec("host -p /workspace ls")

    assert result.exit_code == 0
    assert shell.vfs.get_version("/workspace/app.py") == original_version


def test_urls_not_rewritten():
    shell = setup_shell()
    res = shell.exec("bash -lc 'printf https://example.com'")
    assert "https://example.com" in res.stdout
