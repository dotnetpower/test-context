import json
import sqlite3
import subprocess
import tempfile
import unittest
from pathlib import Path


class ConfigureWindowsProfileTests(unittest.TestCase):
    def test_configures_associated_wsl_profile_without_dropping_settings(self) -> None:
        repository = Path(__file__).resolve().parents[1]
        script = (
            repository
            / "vscode-extension"
            / "scripts"
            / "configure-windows-profile.py"
        )
        with tempfile.TemporaryDirectory() as temporary:
            user_data = Path(temporary)
            database = user_data / "globalStorage" / "state.vscdb"
            database.parent.mkdir()
            with sqlite3.connect(database) as connection:
                connection.execute(
                    "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)"
                )
                connection.execute(
                    "INSERT INTO ItemTable (key, value) VALUES (?, ?)",
                    (
                        "associatedRemoteProfiles",
                        json.dumps({"wsl+Ubuntu-22.04": ["fdai-profile"]}),
                    ),
                )
            profile = user_data / "profiles" / "fdai-profile"
            profile.mkdir(parents=True)
            base_settings_path = user_data / "settings.json"
            base_settings_path.write_text(
                json.dumps({"window.zoomLevel": 1}),
                encoding="utf-8",
            )
            settings_path = profile / "settings.json"
            settings_path.write_text(
                json.dumps(
                    {
                        "editor.fontSize": 14,
                        "remote.extensionKind": {"other.extension": ["workspace"]},
                    }
                ),
                encoding="utf-8",
            )

            subprocess.run(
                [
                    "python3",
                    str(script),
                    "--user-data",
                    str(user_data),
                    "--distribution",
                    "Ubuntu-22.04",
                    "--executable",
                    "/home/test/.local/bin/tctx",
                ],
                check=True,
                capture_output=True,
                text=True,
            )

            settings = json.loads(settings_path.read_text(encoding="utf-8"))
            base_settings = json.loads(
                base_settings_path.read_text(encoding="utf-8")
            )
            self.assertEqual(base_settings["window.zoomLevel"], 1)
            self.assertEqual(
                base_settings["remote.extensionKind"][
                    "moonchoi.test-context-native"
                ],
                ["ui"],
            )
            self.assertEqual(
                base_settings["testContextNative.wslDistribution"], "Ubuntu-22.04"
            )
            self.assertEqual(settings["editor.fontSize"], 14)
            self.assertEqual(
                settings["remote.extensionKind"]["other.extension"], ["workspace"]
            )
            self.assertEqual(
                settings["remote.extensionKind"]["moonchoi.test-context-native"],
                ["ui"],
            )
            self.assertEqual(
                settings["testContextNative.wslDistribution"], "Ubuntu-22.04"
            )
            self.assertEqual(
                settings["testContextNative.wslExecutablePath"],
                "/home/test/.local/bin/tctx",
            )


if __name__ == "__main__":
    unittest.main()
