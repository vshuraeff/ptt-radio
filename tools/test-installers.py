"""Run installer fixtures without executing any installed binary.

Run with uv run -q tools/test-installers.py. Windows runs the PowerShell installer;
macOS and Linux run the POSIX installer. No release or user-data is touched.
"""

import functools
import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import platform
import shutil
import shlex
import subprocess
import tarfile
import tempfile
import threading
import unittest
import zipfile


ROOT = Path(__file__).resolve().parents[1]


def powershell_literal(value):
    return "'" + str(value).replace("'", "''") + "'"


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def do_HEAD(self):
        if self.path == "/releases/latest":
            self.send_response(302)
            self.send_header("Location", getattr(self.server, "latest_location", "/releases/tag/v9.8.7"))
            self.end_headers()
        else:
            super().do_HEAD()

    def do_GET(self):
        if self.path == "/releases/latest":
            self.do_HEAD()
        else:
            super().do_GET()

    def log_message(self, format, *args):
        pass


class InstallerFixtures(unittest.TestCase):
    @unittest.skipIf(os.name == "nt", "POSIX input validation")
    def test_shell_does_not_run_a_truncated_stream(self):
        script = (ROOT / "install.sh").read_text()
        truncated = script[:script.index('factorio_dir=$(CDPATH=')]
        with tempfile.TemporaryDirectory() as directory:
            data = Path(directory) / "untouched"
            result = subprocess.run(["sh", "-s", "--", "--version", "v9.8.7", "--factorio-dir", str(data)], input=truncated, text=True, capture_output=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(data.exists())

    @unittest.skipIf(os.name == "nt", "POSIX input validation")
    def test_shell_rejects_options_as_values(self):
        with tempfile.TemporaryDirectory() as directory:
            for option in ("--factorio-dir", "--mods-dir", "--version"):
                result = subprocess.run(["sh", str(ROOT / "install.sh"), option, "--help"], cwd=directory, text=True, capture_output=True, timeout=5)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("invalid value", result.stderr)
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_install_update_and_failed_downloads(self):
        self.run_fixtures()

    @unittest.skipIf(os.name == "nt", "POSIX archive fixtures")
    def test_linux_archive(self):
        self.run_fixtures("Linux")

    @unittest.skipIf(os.name == "nt", "POSIX archive fixtures")
    def test_macos_archive(self):
        self.run_fixtures("Darwin")

    def run_fixtures(self, fixture_system=None):
        windows = os.name == "nt"
        if windows:
            executable = shutil.which("pwsh") or shutil.which("powershell")
            if not executable:
                self.skipTest("PowerShell is unavailable")
            command = [executable, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(ROOT / "install.ps1")]
            options = ["-Version", "v9.8.7", "-FactorioDir"]
            mods_option = "-ModsDir"
            binary = "ptt-radio-companion.exe"
            suffix = "windows-x86_64.zip"
        else:
            command = ["sh", str(ROOT / "install.sh")]
            options = ["--version", "v9.8.7", "--factorio-dir"]
            mods_option = "--mods-dir"
            binary = "ptt-radio-companion"
            suffix = "macos-universal.zip" if (fixture_system or platform.system()) == "Darwin" else "linux-x86_64.tar.gz"
        with tempfile.TemporaryDirectory(prefix="ptt installer fixtures ") as temporary:
            base = Path(temporary).resolve()
            releases = base / "releases" / "download" / "v9.8.7"
            releases.mkdir(parents=True)
            tags = base / "releases" / "tag"
            tags.mkdir()
            (tags / "v9.8.7").write_text("fixture release", encoding="ascii")
            shutil.copyfile(ROOT / "install.sh", base / "install.sh")
            shutil.copyfile(ROOT / "install.ps1", base / "install.ps1")
            data = base / ("Factorio user's $data \\\nline" if not windows else "Factorio user's $data")
            mods = base / ('a' * 64 + ' custom mods "quoted" \\ путь\n' if not windows else "custom mods 'quoted'")
            mods.mkdir()
            mod_list = mods / "mod-list.json"
            mod_list.write_text('{"mods": []}', encoding="utf-8")
            asset = releases / f"ptt-radio-companion-9.8.7-{suffix}"
            sums = releases / "SHA256SUMS"

            def archive(contents, unsafe=False):
                files = {binary: contents, "LICENSE": b"fixture license", "README.md": b"fixture readme"}
                if unsafe:
                    files["../escape"] = b"must never be extracted"
                if suffix == "macos-universal.zip":
                    files["._ptt-radio-companion"] = b"fixture AppleDouble"
                    files["__MACOSX/._README.md"] = b"fixture metadata"
                if suffix.endswith("zip"):
                    with zipfile.ZipFile(asset, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
                        for name, value in files.items():
                            bundle.writestr(name, value)
                else:
                    with tarfile.open(asset, "w:gz") as bundle:
                        for name, value in files.items():
                            entry = tarfile.TarInfo(name)
                            entry.size = len(value)
                            bundle.addfile(entry, io.BytesIO(value))
                digest = hashlib.sha256(asset.read_bytes()).hexdigest()
                sums.write_text(f"{digest}  {asset.name}\n", encoding="ascii")

            handler = functools.partial(QuietHandler, directory=str(base))
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                environment = dict(os.environ, PTT_RADIO_INSTALL_TEST_RELEASE_URL=f"http://127.0.0.1:{server.server_port}/releases")
                if fixture_system:
                    commands = base / "commands"
                    commands.mkdir()
                    uname = commands / "uname"
                    uname.write_text(f"#!/bin/sh\ncase \"$1\" in -s) printf '%s\\n' {fixture_system} ;; -m) printf '%s\\n' x86_64 ;; *) exit 1 ;; esac\n", encoding="ascii")
                    uname.chmod(0o755)
                    environment["PATH"] = str(commands) + os.pathsep + environment["PATH"]

                def run(success, pipe=False, relative=False, move_failure=None):
                    arguments = options + [str(data), mods_option, str(mods)]
                    invocation_environment = environment.copy()
                    if move_failure and not windows:
                        commands = base / ("fail-move-" + move_failure)
                        commands.mkdir(exist_ok=True)
                        move = commands / "mv"
                        reject = "*/new|*/previous" if move_failure == "rollback" else "*/new"
                        move.write_text(f'#!/bin/sh\ncase "$1" in {reject}) exit 1 ;; esac\nexec {shlex.quote(shutil.which("mv"))} "$@"\n')
                        move.chmod(0o755)
                        invocation_environment["PATH"] = str(commands) + os.pathsep + environment["PATH"]
                    if pipe and not windows:
                        download = subprocess.Popen(["curl", "-fsSL", f"http://127.0.0.1:{server.server_port}/install.sh"], stdout=subprocess.PIPE)
                        try:
                            result = subprocess.run(["sh", "-s", "--"] + arguments[2:], stdin=download.stdout, env=invocation_environment, text=True, capture_output=True, timeout=30)
                        finally:
                            download.stdout.close()
                            self.assertEqual(download.wait(timeout=5), 0)
                    elif pipe:
                        entry = base / "pipe-install.ps1"
                        entry.write_text(f"$env:APPDATA = {powershell_literal(base / 'pipe home')}\nirm http://127.0.0.1:{server.server_port}/install.ps1 | iex\n", encoding="utf-8")
                        result = subprocess.run(command[:4] + ["-File", str(entry)], env=invocation_environment, text=True, capture_output=True, timeout=30)
                    elif windows and (relative or move_failure):
                        entry = base / "wrapped-install.ps1"
                        script = ["$ErrorActionPreference = 'Stop'"]
                        if relative:
                            process_dir = base / "different process directory"
                            process_dir.mkdir(exist_ok=True)
                            script += [f"Set-Location -LiteralPath {powershell_literal(base)}", f"[Environment]::CurrentDirectory = {powershell_literal(process_dir)}"]
                            arguments = options + [data.name, mods_option, mods.name]
                        if move_failure:
                            script += ["function Move-Item {", "param([string]$LiteralPath, [string]$Destination)", "$leaf = Split-Path -Path $LiteralPath -Leaf", f"if ($leaf -eq 'new' -or ({powershell_literal(move_failure)} -eq 'rollback' -and $leaf -eq 'previous')) {{ throw 'fixture move failure' }}", "Microsoft.PowerShell.Management\\Move-Item -LiteralPath $LiteralPath -Destination $Destination", "}"]
                        script += ["& " + powershell_literal(ROOT / "install.ps1") + " " + " ".join(argument if index % 2 == 0 else powershell_literal(argument) for index, argument in enumerate(arguments))]
                        entry.write_text("\n".join(script), encoding="utf-8")
                        result = subprocess.run(command[:4] + ["-File", str(entry)], env=invocation_environment, text=True, capture_output=True, timeout=30)
                    else:
                        result = subprocess.run(command + arguments, env=invocation_environment, text=True, capture_output=True, timeout=30)
                    self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
                    return result.stdout + result.stderr

                archive(b"fixture version one, never execute")
                output = run(True)
                installed = data / "ptt-radio-companion"
                self.assertIn("--launch %command%", output)
                self.assertIn("Start:", output)
                if not windows:
                    start = output.partition("Start: ")[2].partition("\nSteam launch options: ")[0]
                    self.assertEqual(shlex.split(start), [str(installed / binary)])
                    steam = output.partition("Steam launch options: ")[2].removesuffix("\n")
                    quoted_command = base / "check-quoting.sh"
                    quoted_command.write_text('set -- ' + steam + '\nprintf \'%s\\0\' "$@"\n', encoding="utf-8")
                    parsed = subprocess.run(["sh", str(quoted_command)], check=True, capture_output=True, timeout=5)
                    self.assertEqual(parsed.stdout.decode().split("\0")[:-1], [str(installed / binary), "--launch", "%command%"])
                if windows:
                    run(True, relative=True)
                self.assertEqual(json.loads((installed / "install.json").read_text()), {"version": 1, "mods_dir": str(mods), "installed_by": "ptt-radio installer"})
                self.assertEqual({file.name for file in installed.iterdir()}, {binary, "LICENSE", "README.md", "install.json"})
                run(True, pipe=True)
                if windows:
                    pipe_install = base / "pipe home" / "Factorio" / "ptt-radio-companion"
                    self.assertEqual((pipe_install / binary).read_bytes(), b"fixture version one, never execute")
                server.latest_location = f"http://127.0.0.1:{server.server_port}/releases/tag/v9.8.7"
                run(True, pipe=True)
                server.latest_location = "/unexpected-redirect"
                run(False, pipe=True)
                del server.latest_location
                archive(b"fixture version two, never execute")
                run(True)
                before = {file.name: file.read_bytes() for file in installed.iterdir()}
                self.assertEqual(before[binary], b"fixture version two, never execute")
                run(False, move_failure="promotion")
                self.assertEqual({file.name: file.read_bytes() for file in installed.iterdir()}, before)
                self.assertEqual(list(data.glob(".ptt-radio-install*")), [])
                failure_output = run(False, move_failure="rollback")
                self.assertFalse(installed.exists())
                retained = list(data.glob(".ptt-radio-install*"))
                self.assertEqual(len(retained), 1)
                self.assertIn(str(retained[0] / "previous"), failure_output)
                self.assertEqual({file.name: file.read_bytes() for file in (retained[0] / "previous").iterdir()}, before)
                shutil.move(str(retained[0] / "previous"), installed)
                shutil.rmtree(retained[0])
                asset.write_bytes(b"tampered")
                run(False)
                self.assertEqual({file.name: file.read_bytes() for file in installed.iterdir()}, before)
                archive(b"fixture valid payload")
                compressed = asset.read_bytes()
                if suffix.endswith("zip"):
                    with zipfile.ZipFile(asset) as bundle:
                        entry = bundle.getinfo(binary)
                    start = entry.header_offset + 30 + len(entry.filename.encode()) + len(entry.extra)
                    broken = compressed[:start] + b"\xff" * entry.compress_size + compressed[start + entry.compress_size:]
                else:
                    broken = compressed[:len(compressed) // 2]
                self.assertNotEqual(broken, compressed)
                asset.write_bytes(broken)
                sums.write_text(f"{hashlib.sha256(broken).hexdigest()}  {asset.name}\n", encoding="ascii")
                run(False)
                self.assertEqual({file.name: file.read_bytes() for file in installed.iterdir()}, before)
                archive(b"invalid layout", unsafe=True)
                run(False)
                self.assertEqual({file.name: file.read_bytes() for file in installed.iterdir()}, before)
                self.assertFalse((data / "escape").exists())
                asset.unlink()
                run(False)
                self.assertEqual({file.name: file.read_bytes() for file in installed.iterdir()}, before)
                self.assertEqual(mod_list.read_text(), '{"mods": []}')
                self.assertEqual(list(data.glob(".ptt-radio-install*")), [])
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)


if __name__ == "__main__":
    unittest.main()
