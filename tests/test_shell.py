from sandfs import NodePolicy, SandboxShell, VirtualFileSystem


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
    res = shell.exec("python -c \"print(len(vfs.ls('/workspace')))\"")
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


def test_host_dashdash_sets_cwd_and_preserves_args():
    shell = setup_shell()
    sentinel_name = ".cwd-sentinel"
    shell.vfs.write_file(f"/workspace/{sentinel_name}", "marker")

    res = shell.exec("host -C /workspace -- ls -a")
    assert res.exit_code == 0
    assert sentinel_name in res.stdout

    dash_res = shell.exec("host -C /workspace -- --version")
    assert dash_res.exit_code == 127
    assert "'--version'" in dash_res.stderr


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
    res = shell.exec("doesnotexist")
    assert res.exit_code == 127


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
    shell.host_fallback = True
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
    assert res.exit_code != 0


def test_ls_unreadable_directory_errors():
    vfs = VirtualFileSystem()
    vfs.mkdir("/secret")
    vfs.write_file("/secret/a.txt", "nope")
    vfs.set_policy("/secret", NodePolicy(readable=False))
    shell = SandboxShell(vfs)
    res = shell.exec("ls /secret")
    assert res.exit_code == 1
    assert "not readable" in res.stderr.lower()


def test_pipe_and_grep_from_stdin():
    shell = setup_shell()
    res = shell.exec('printf "a\\\\nb" | grep a')
    assert res.stdout.strip() == "a"


def test_redirection_and_cat_stdin():
    shell = setup_shell()
    shell.exec("echo hi > /notes/a.txt")
    res = shell.exec("cat < /notes/a.txt")
    assert res.stdout.strip() == "hi"


def test_glob_expansion():
    shell = setup_shell()
    res = shell.exec("ls /workspace/*.py")
    assert "app.py" in res.stdout


def test_env_assignment_expands():
    shell = setup_shell()
    res = shell.exec("FOO=bar echo $FOO")
    assert res.stdout.strip() == "bar"


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


def test_host_rm_removes_missing_subtree():
    shell = setup_shell()
    shell.exec("mkdir /workspace/tmp")
    shell.exec("mkdir /workspace/tmp/sub")
    shell.exec("write /workspace/tmp/sub/nested.txt hi")

    result = shell.exec("host -p /workspace rm -rf tmp")

    assert result.exit_code == 0
    assert shell.vfs.exists("/workspace/tmp") is False
    assert shell.vfs.exists("/workspace/tmp/sub/nested.txt") is False


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


def test_host_command_preserves_trailing_slash_paths():
    shell = setup_shell()
    shell.exec("mkdir /blue")

    cmd = "bash -lc 'fname=/blue/inbox/; mkdir -p \"$fname\"; printf hi > ${fname}note.txt'"
    res = shell.exec(cmd)

    assert res.exit_code == 0
    assert not shell.vfs.exists("/blue/inboxnote.txt")
    assert shell.vfs.exists("/blue/inbox/note.txt")
    assert shell.vfs.read_file("/blue/inbox/note.txt") == "hi"


def test_mv_moves_file_into_directory():
    shell = setup_shell()
    shell.exec("mkdir /blue")
    shell.exec("mkdir /blue/inbox")
    shell.exec("write /workspace/note.txt hi")

    result = shell.exec("mv /workspace/note.txt /blue/inbox/")

    assert result.exit_code == 0
    assert not shell.vfs.exists("/workspace/note.txt")
    assert shell.vfs.read_file("/blue/inbox/note.txt") == "hi"


def test_mv_renames_file():
    shell = setup_shell()

    result = shell.exec("mv /workspace/app.py /workspace/renamed.py")

    assert result.exit_code == 0
    assert not shell.vfs.exists("/workspace/app.py")
    assert shell.vfs.read_file("/workspace/renamed.py") == "print('hi')\n"


def test_mv_moves_directories():
    shell = setup_shell()
    shell.exec("mkdir /blue")
    shell.exec("mkdir /blue/inbox")
    shell.exec("write /blue/inbox/note.txt hi")

    result = shell.exec("mv /blue/inbox /workspace/messages")

    assert result.exit_code == 0
    assert not shell.vfs.exists("/blue/inbox")
    assert shell.vfs.read_file("/workspace/messages/note.txt") == "hi"


def test_cp_copies_file_to_new_name():
    shell = setup_shell()
    shell.host_fallback = False

    result = shell.exec("cp /workspace/app.py /workspace/app_copy.py")

    assert result.exit_code == 0
    assert shell.vfs.read_file("/workspace/app.py") == "print('hi')\n"
    assert shell.vfs.read_file("/workspace/app_copy.py") == "print('hi')\n"


def test_cp_copies_file_into_directory():
    shell = setup_shell()
    shell.host_fallback = False
    shell.exec("mkdir /blue")
    shell.exec("mkdir /blue/inbox")

    result = shell.exec("cp /workspace/README.md /blue/inbox/")

    assert result.exit_code == 0
    assert shell.vfs.read_file("/blue/inbox/README.md") == "hello world\n"


def test_cp_copies_directory_with_recursive_flag():
    shell = setup_shell()
    shell.host_fallback = False
    shell.exec("mkdir /blue")
    shell.exec("mkdir /blue/inbox")
    shell.exec("write /blue/inbox/note.txt hi")

    result = shell.exec("cp -r /blue /workspace/archive")

    assert result.exit_code == 0
    assert shell.vfs.read_file("/workspace/archive/inbox/note.txt") == "hi"
    assert shell.vfs.exists("/blue/inbox/note.txt")
